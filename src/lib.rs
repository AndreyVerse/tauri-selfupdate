//! Самообновление portable-exe из релизов GitVerse, открытого или приватного репозитория GitHub.
//!
//! В релизе лежат архив `<App>.zip` с exe и манифест `latest.txt`:
//! `{"version": "1.2.3", "notes": "...", "exe": {"signature": "<minisign архива, base64>"}}`.
//! Exe и `.json` GitVerse во вложения не принимает, поэтому архив и `.txt`.
//! [`check`] находит последний релиз и читает манифест, [`install`] качает архив рядом с
//! работающим exe, сверяет размер и minisign-подпись, распаковывает, подменяет
//! переименованием и откатывается при сбое.
//! Затем приложение зовёт [`relaunch`] и выходит; новый процесс первым делом зовёт [`startup`].
//!
//! Все функции блокирующие: из async-кода их зовут через `spawn_blocking`.

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Флаг командной строки перезапущенного после обновления процесса.
pub const AFTER_UPDATE_ARG: &str = "--after-update";

/// Имя манифеста во вложениях релиза.
pub const MANIFEST_ASSET: &str = "latest.txt";

/// Анонимный API веб-хоста: `api.gitverse.ru` требует токен даже для публичных репозиториев.
const GITVERSE_API: &str = "https://gitverse.ru/api/repos";
const GITHUB_API: &str = "https://api.github.com/";

/// Где лежат релизы.
#[derive(Clone)]
pub enum Source {
    /// Публичный репозиторий `owner/name` на gitverse.ru, без токена.
    GitVerse { repo: String },
    /// Приватный репозиторий `owner/name` на github.com с релизами нескольких приложений:
    /// теги `<tag_prefix>-v1.2.3`. Токен fine-grained, только чтение содержимого этого
    /// репозитория: он вшит в exe, поэтому из exe его можно достать.
    GitHubPrivate { repo: String, token: String, tag_prefix: String },
    /// Открытый репозиторий `owner/name` на github.com, без токена: теги `v1.2.3`.
    /// Манифест и архив берутся по ссылкам `releases/latest/download/…` и
    /// `releases/download/v<версия>/…`, а не через API: анонимный API даёт 60
    /// запросов в час на IP, и за общим NAT проверка падала бы с 403.
    GitHub { repo: String },
}

// Токен не печатается: Debug конфигурации может попасть в журнал приложения.
impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitVerse { repo } => write!(f, "GitVerse({repo})"),
            Self::GitHubPrivate { repo, tag_prefix, .. } => write!(f, "GitHubPrivate({repo}, {tag_prefix})"),
            Self::GitHub { repo } => write!(f, "GitHub({repo})"),
        }
    }
}

/// Где лежат релизы приложения и каким ключом их проверять.
#[derive(Debug, Clone)]
pub struct Config {
    pub source: Source,
    /// Имя exe внутри архива, например `App.exe`; архив называется так же, но `.zip`.
    pub asset: String,
    /// Публичный minisign-ключ в виде, как его пишет `tauri signer generate` (base64 файла `.pub`).
    pub public_key: String,
    pub user_agent: String,
}

impl Config {
    pub fn archive_name(&self) -> String {
        let stem = self.asset.strip_suffix(".exe").unwrap_or(&self.asset);
        format!("{stem}.zip")
    }

    fn repo(&self) -> &str {
        match &self.source {
            Source::GitVerse { repo } | Source::GitHubPrivate { repo, .. } | Source::GitHub { repo } => repo,
        }
    }
}

/// Для проверки на локальном сервере; подменить этим exe нельзя — подпись
/// всё равно сверяется вшитым публичным ключом.
fn manifest_override() -> Option<String> {
    std::env::var("SELFUPDATE_MANIFEST_URL").ok().filter(|url| !url.trim().is_empty())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub notes: Option<String>,
    pub exe_url: String,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Progress {
    pub downloaded: u64,
    /// 0, если сервер не прислал `Content-Length`.
    pub total: u64,
}

/// `kind` стабилен и нужен UI для локализованного текста; `message` — техническая подробность.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    /// Нет связи, DNS, TLS или таймаут.
    Network,
    /// Сервер ответил ошибкой (429 — лимит запросов GitVerse).
    Http,
    /// Нет релиза, манифеста или архива; манифест не JSON или без `version`; в архиве нет exe.
    Manifest,
    /// В манифесте нет подписи exe: ставить его значило бы верить вслепую.
    NoSignature,
    /// Подпись не сошлась со скачанными байтами или ключом.
    Signature,
    /// Загрузка оборвалась раньше `Content-Length`.
    Incomplete,
    /// Диск: создание, запись, переименование, блокировка файлов.
    Io,
    /// Установленная версия не старше последнего релиза.
    UpToDate,
    /// В этом процессе уже идёт установка.
    Busy,
}

impl UpdateError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for UpdateError {}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    notes: Option<String>,
    exe: Option<ManifestExe>,
}

#[derive(Deserialize)]
struct ManifestExe {
    url: Option<String>,
    signature: Option<String>,
}

#[derive(Deserialize)]
struct ReleaseList {
    #[serde(default)]
    data: Vec<Release>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Release {
    tag_name: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    is_prerelease: bool,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    name: String,
    download_url: String,
}

impl Release {
    fn attachment(&self, name: &str) -> Option<&str> {
        self.attachments.iter().find(|a| a.name == name).map(|a| a.download_url.as_str())
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    /// Адрес API: у приватного репозитория `browser_download_url` без токена не открывается.
    url: String,
}

/// Публичная ссылка на вложение релиза открытого репозитория. `None` — последний
/// релиз (GitHub сам выбирает последний не-черновик и не-prerelease).
fn github_public_url(repo: &str, version: Option<&str>, file: &str) -> String {
    match version {
        None => format!("https://github.com/{repo}/releases/latest/download/{file}"),
        Some(version) => format!("https://github.com/{repo}/releases/download/v{version}/{file}"),
    }
}

/// Релизы одного приложения из общего репозитория; тег без префикса — версия.
fn github_releases(list: Vec<GitHubRelease>, tag_prefix: &str) -> Vec<Release> {
    let prefix = format!("{tag_prefix}-");
    list.into_iter()
        .filter_map(|r| {
            Some(Release {
                tag_name: r.tag_name.strip_prefix(&prefix)?.to_string(),
                is_draft: r.draft,
                is_prerelease: r.prerelease,
                attachments: r.assets.into_iter().map(|a| Attachment { name: a.name, download_url: a.url }).collect(),
            })
        })
        .collect()
}

/// Порядок выдачи API не документирован, поэтому старший релиз ищется по версии тега.
fn latest_release(releases: Vec<Release>) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| !r.is_draft && !r.is_prerelease)
        .reduce(|best, r| if is_newer(&r.tag_name, &best.tag_name) { r } else { best })
}

fn agent(body_timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::NativeTls).build();
    // Таймауты по фазам, а не общий: медленный канал не должен рвать загрузку на середине,
    // а мёртвый сервер — вешать проверку при старте.
    ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(body_timeout))
        .build()
        .into()
}

fn get(agent: &ureq::Agent, url: &str, config: &Config) -> Result<ureq::http::Response<ureq::Body>, UpdateError> {
    let mut request = agent.get(url).header("User-Agent", &config.user_agent);
    // Токен — только для API GitHub. Вложение отдаётся редиректом на подписанный адрес
    // хранилища, и ureq по умолчанию (`RedirectAuthHeaders::Never`) туда его не передаёт.
    if let Source::GitHubPrivate { token, .. } = &config.source {
        if url.starts_with(GITHUB_API) {
            let accept = if url.contains("/releases/assets/") { "application/octet-stream" } else { "application/vnd.github+json" };
            request = request
                .header("Authorization", &format!("Bearer {token}"))
                .header("Accept", accept)
                .header("X-GitHub-Api-Version", "2022-11-28");
        }
    }
    let response = request.call().map_err(|e| UpdateError::new(ErrorKind::Network, format!("{url}: {e}")))?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(UpdateError::new(ErrorKind::Http, format!("{url}: HTTP {status}")));
    }
    Ok(response)
}

fn read_json<T: serde::de::DeserializeOwned>(agent: &ureq::Agent, url: &str, config: &Config) -> Result<T, UpdateError> {
    get(agent, url, config)?
        .body_mut()
        .read_json()
        .map_err(|e| UpdateError::new(ErrorKind::Manifest, format!("{url}: {e}")))
}

/// Читает манифест последнего релиза и сравнивает версии.
pub fn check(config: &Config, current_version: &str) -> Result<UpdateInfo, UpdateError> {
    let agent = agent(Duration::from_secs(30));
    if let Some(url) = manifest_override() {
        let manifest: Manifest = read_json(&agent, &url, config)?;
        return info_from(current_version, manifest, None);
    }
    if let Source::GitHub { repo } = &config.source {
        let manifest: Manifest = read_json(&agent, &github_public_url(repo, None, MANIFEST_ASSET), config)?;
        // Архив — по ссылке с версией из манифеста, а не `latest`: новый релиз между
        // проверкой и установкой подменил бы архив под старой подписью.
        let version = manifest.version.trim().trim_start_matches('v').to_string();
        let archive_url = github_public_url(repo, Some(&version), &config.archive_name());
        return info_from(current_version, manifest, Some(&archive_url));
    }
    let releases = match &config.source {
        Source::GitVerse { repo } => read_json::<ReleaseList>(&agent, &format!("{GITVERSE_API}/{repo}/releases"), config)?.data,
        Source::GitHubPrivate { repo, tag_prefix, .. } => {
            let list = read_json(&agent, &format!("{GITHUB_API}repos/{repo}/releases?per_page=100"), config)?;
            github_releases(list, tag_prefix)
        }
        Source::GitHub { .. } => unreachable!("открытый GitHub обработан выше"),
    };
    let release = latest_release(releases)
        .ok_or_else(|| UpdateError::new(ErrorKind::Manifest, format!("{}: no releases", config.repo())))?;
    let missing = |name: String| UpdateError::new(ErrorKind::Manifest, format!("{}: no {name} in release", release.tag_name));
    let manifest_url = release.attachment(MANIFEST_ASSET).ok_or_else(|| missing(MANIFEST_ASSET.into()))?;
    let archive = config.archive_name();
    let archive_url = release.attachment(&archive).ok_or_else(|| missing(archive.clone()))?;
    let manifest: Manifest = read_json(&agent, manifest_url, config)?;
    info_from(current_version, manifest, Some(archive_url))
}

/// Адрес из манифеста важнее найденного в релизе: так манифест на локальном сервере
/// может указать на локальный архив.
fn info_from(current_version: &str, manifest: Manifest, archive_url: Option<&str>) -> Result<UpdateInfo, UpdateError> {
    let version = manifest.version.trim_start_matches('v').to_string();
    let exe = manifest.exe.unwrap_or(ManifestExe { url: None, signature: None });
    let exe_url = exe
        .url
        .or_else(|| archive_url.map(str::to_owned))
        .ok_or_else(|| UpdateError::new(ErrorKind::Manifest, "manifest has no archive url"))?;
    Ok(UpdateInfo {
        available: is_newer(&version, current_version),
        current_version: current_version.to_string(),
        exe_url,
        latest_version: version,
        notes: manifest.notes.filter(|n| !n.trim().is_empty()),
        signature: exe.signature,
    })
}

/// Порядок SemVer: `1.2.0-rc.1 < 1.2.0 < 1.2.1`; метаданные сборки (`+build`) не учитываются.
pub fn is_newer(remote: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<(Vec<u64>, &str)> {
        let v = v.trim().trim_start_matches('v');
        let v = v.split_once('+').map_or(v, |(core, _)| core);
        let (core, pre) = v.split_once('-').unwrap_or((v, ""));
        let parts = core.split('.').map(|p| p.parse().ok()).collect::<Option<Vec<u64>>>()?;
        Some((parts, pre))
    }
    let (Some((mut r, r_pre)), Some((mut c, c_pre))) = (parse(remote), parse(current)) else {
        return false;
    };
    let len = r.len().max(c.len());
    r.resize(len, 0);
    c.resize(len, 0);
    match r.cmp(&c) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => prerelease_cmp(r_pre, c_pre).is_gt(),
    }
}

fn prerelease_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    let (mut left, mut right) = (a.split('.'), b.split('.'));
    loop {
        let order = match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            },
        };
        if order.is_ne() {
            return order;
        }
    }
}

/// Проверка minisign-подписи. Ключ и подпись — base64 поверх текста minisign,
/// ровно как их пишет `tauri signer`.
pub fn verify_signature(public_key_b64: &str, signature_b64: &str, data: &[u8]) -> Result<(), UpdateError> {
    use base64::Engine;
    let decode = |value: &str, what: &str| -> Result<String, UpdateError> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .map_err(|e| UpdateError::new(ErrorKind::Signature, format!("{what} is not base64: {e}")))?;
        String::from_utf8(raw).map_err(|e| UpdateError::new(ErrorKind::Signature, format!("{what} is not text: {e}")))
    };
    let key = minisign_verify::PublicKey::decode(&decode(public_key_b64, "public key")?)
        .map_err(|e| UpdateError::new(ErrorKind::Signature, format!("public key rejected: {e}")))?;
    let signature = minisign_verify::Signature::decode(&decode(signature_b64, "signature")?)
        .map_err(|e| UpdateError::new(ErrorKind::Signature, format!("signature rejected: {e}")))?;
    key.verify(data, &signature, false)
        .map_err(|e| UpdateError::new(ErrorKind::Signature, format!("signature does not match: {e}")))
}

/// Качает, проверяет и ставит новый exe. После успеха работающий процесс живёт
/// из переименованного `.old`, пока приложение не позовёт [`relaunch`] и не выйдет.
pub fn install(config: &Config, info: &UpdateInfo, on_progress: impl FnMut(Progress)) -> Result<(), UpdateError> {
    // Две параллельные установки писали бы в один `.exe.new` и склеили бы загрузки.
    static INSTALL: Mutex<()> = Mutex::new(());
    let _guard = INSTALL
        .try_lock()
        .map_err(|_| UpdateError::new(ErrorKind::Busy, "an update is already being installed"))?;
    if !info.available {
        return Err(UpdateError::new(ErrorKind::UpToDate, info.current_version.clone()));
    }
    // Без подписи подменённый по пути exe уехал бы пользователю как обычное обновление.
    let signature = info
        .signature
        .as_deref()
        .ok_or_else(|| UpdateError::new(ErrorKind::NoSignature, info.exe_url.clone()))?;
    let current = std::env::current_exe().map_err(|e| UpdateError::new(ErrorKind::Io, e.to_string()))?;
    let paths = Paths::for_exe(&current);

    // Подписан архив целиком: распаковка идёт только из проверенных байтов.
    let downloaded = download(config, &info.exe_url, &paths.archive, on_progress).and_then(|()| {
        let bytes = fs::read(&paths.archive).map_err(|e| UpdateError::new(ErrorKind::Io, e.to_string()))?;
        verify_signature(&config.public_key, signature, &bytes)?;
        unpack(&bytes, &config.asset, &paths.new)
    });
    let _ = fs::remove_file(&paths.archive);
    if let Err(e) = downloaded {
        // Недокачанный или чужой файл не должен лежать рядом с программой.
        let _ = fs::remove_file(&paths.new);
        return Err(e);
    }
    swap(&paths)
}

struct Paths {
    current: PathBuf,
    archive: PathBuf,
    new: PathBuf,
    old: PathBuf,
}

impl Paths {
    fn for_exe(current: &Path) -> Self {
        Self {
            current: current.to_path_buf(),
            archive: current.with_extension("zip.new"),
            new: current.with_extension("exe.new"),
            old: current.with_extension("exe.old"),
        }
    }
}

fn unpack(archive: &[u8], name: &str, target: &Path) -> Result<(), UpdateError> {
    let bad = |e: String| UpdateError::new(ErrorKind::Manifest, format!("archive: {e}"));
    let io = |e: std::io::Error| UpdateError::new(ErrorKind::Io, format!("{}: {e}", target.display()));
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|e| bad(e.to_string()))?;
    let mut entry = zip.by_name(name).map_err(|e| bad(format!("{name}: {e}")))?;
    let mut file = fs::File::create(target).map_err(io)?;
    std::io::copy(&mut entry, &mut file).map_err(io)?;
    file.sync_all().map_err(io)
}

fn download(config: &Config, url: &str, target: &Path, mut on_progress: impl FnMut(Progress)) -> Result<(), UpdateError> {
    let io = |e: std::io::Error| UpdateError::new(ErrorKind::Io, format!("{}: {e}", target.display()));
    let response = get(&agent(Duration::from_secs(15 * 60)), url, config)?;
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(target).map_err(io)?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut downloaded = 0u64;
    let mut reported = Instant::now();
    on_progress(Progress { downloaded, total });
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| UpdateError::new(ErrorKind::Network, format!("{url}: {e}")))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(io)?;
        downloaded += read as u64;
        // Прогресс не чаще 10 раз в секунду: каждый вызов в приложении — событие в UI.
        if reported.elapsed() >= Duration::from_millis(100) {
            on_progress(Progress { downloaded, total });
            reported = Instant::now();
        }
    }
    // rename без fsync после сбоя питания мог оставить на месте программы пустой файл.
    file.sync_all().map_err(io)?;
    on_progress(Progress { downloaded, total });
    if total > 0 && downloaded != total {
        return Err(UpdateError::new(ErrorKind::Incomplete, format!("{downloaded} of {total} bytes")));
    }
    Ok(())
}

/// Windows разрешает переименовать запущенный exe, но не удалить и не перезаписать его.
fn swap(paths: &Paths) -> Result<(), UpdateError> {
    let io = |what: &str, e: std::io::Error| UpdateError::new(ErrorKind::Io, format!("{what}: {e}"));
    if let Err(e) = fs::remove_file(&paths.old) {
        if paths.old.exists() {
            return Err(io(&format!("{} is locked", paths.old.display()), e));
        }
    }
    fs::rename(&paths.current, &paths.old).map_err(|e| io("rename the running exe", e))?;
    if let Err(e) = fs::rename(&paths.new, &paths.current) {
        return Err(match fs::rename(&paths.old, &paths.current) {
            Ok(()) => io("put the new exe in place", e),
            // Программы на диске не осталось — текст ошибки обязан сказать, откуда её вернуть.
            Err(rollback) => UpdateError::new(
                ErrorKind::Io,
                format!(
                    "put the new exe in place: {e}; rollback failed: {rollback}; restore {} from {}",
                    paths.current.display(),
                    paths.old.display()
                ),
            ),
        });
    }
    Ok(())
}

/// Запускает только что поставленный exe; вызывающий сразу после этого выходит.
pub fn relaunch() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.with_file_name(original_name(&exe));
    let mut command = std::process::Command::new(&exe);
    command.arg(AFTER_UPDATE_ARG);
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    command.spawn().map(drop)
}

/// `current_exe` у переименованного процесса может вернуть `X.exe.old`; перезапускаем `X.exe`.
fn original_name(exe: &Path) -> String {
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    name.strip_suffix(".old").unwrap_or(name).to_string()
}

/// Звать первым делом в `main`, до плагина single-instance. После обновления ждёт выхода
/// прежнего процесса (его `.old` становится удаляемым), иначе новый экземпляр принял бы
/// его за уже запущенную копию и закрылся; без обновления просто убирает старый `.old`.
pub fn startup() {
    let Ok(current) = std::env::current_exe() else { return };
    let paths = Paths::for_exe(&current);
    let _ = fs::remove_file(&paths.new);
    if !std::env::args().any(|a| a == AFTER_UPDATE_ARG) {
        let _ = fs::remove_file(&paths.old);
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match fs::remove_file(&paths.old) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            // Файл занят ещё живым старым процессом.
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Одноразовый ключ, сгенерированный только для этих тестов (`tauri signer generate`).
    const TEST_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDgzMTA5OTlEMUU3QzYwNEIKUldSTFlId2VuWmtRZzNRaWdxc2d2Z0xtTnloWkxqTDFmbnoraWsxeUQ3eFdhaTdvQ0tVT3U1TjMK";
    const TEST_SIG: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVSTFlId2VuWmtRZzdBODFUOC9DMVBmWENUTFg2dHYrbjhqdTRuTmNSUUJwY2J6NXBRbjZlYytNVjZhZnZ2Ni9jUDBoVDZ6R0dKN2pHeTltYTYvYlRTSER5eWRKZVBXTUE4PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk0MDc1ODUxCWZpbGU6cGF5bG9hZC5iaW4KRWFaSU8wdVR6NllNbjZvQWVVWk56NjlyN2U4NExicVJIZHJ1MXVoaGVPd0toNGNrb2doUm9uY1BjT1M2TnNZMXh4QW9tVnRySlZBNDJsQXprbG0yQlE9PQo=";
    const PAYLOAD: &[u8] = b"hello selfupdate\n";

    fn config() -> Config {
        Config {
            source: Source::GitVerse { repo: "owner/app".into() },
            asset: "App.exe".into(),
            public_key: TEST_KEY.into(),
            user_agent: "test".into(),
        }
    }

    #[test]
    fn versions_follow_semver() {
        assert!(is_newer("1.2.2", "1.2.1"));
        assert!(is_newer("v1.10.0", "1.9.9"));
        assert!(is_newer("1.2.0", "1.2.0-rc.2"));
        assert!(is_newer("1.2.0-rc.10", "1.2.0-rc.2"));
        assert!(is_newer("1.3", "1.2.9"));
        assert!(!is_newer("1.2.1", "1.2.1"));
        assert!(!is_newer("1.2.1+build.7", "1.2.1"));
        assert!(!is_newer("1.2.0", "1.2.1"));
        assert!(!is_newer("garbage", "1.0.0"));
    }

    #[test]
    fn signature_accepts_signed_bytes_only() {
        assert!(verify_signature(TEST_KEY, TEST_SIG, PAYLOAD).is_ok());
        let tampered = verify_signature(TEST_KEY, TEST_SIG, b"hello selfupdatE\n").unwrap_err();
        assert_eq!(tampered.kind, ErrorKind::Signature);
        assert_eq!(verify_signature("not base64!", TEST_SIG, PAYLOAD).unwrap_err().kind, ErrorKind::Signature);
    }

    #[test]
    fn manifest_without_url_points_at_release_archive() {
        let manifest: Manifest = serde_json::from_str(r#"{"version":"v2.0.0","notes":" ","exe":{"signature":"x"}}"#).unwrap();
        let info = info_from("1.0.0", manifest, Some("https://gitverse.ru/api/attachments/u")).unwrap();
        assert!(info.available);
        assert_eq!(info.latest_version, "2.0.0");
        assert_eq!(info.exe_url, "https://gitverse.ru/api/attachments/u");
        assert_eq!(info.notes, None);
        assert_eq!(info.signature.as_deref(), Some("x"));
        let bare: Manifest = serde_json::from_str(r#"{"version":"2.0.0"}"#).unwrap();
        assert_eq!(info_from("1.0.0", bare, None).unwrap_err().kind, ErrorKind::Manifest);
    }

    #[test]
    fn newest_published_release_wins_regardless_of_order() {
        let list: ReleaseList = serde_json::from_str(
            r#"{"total":4,"data":[
                {"tagName":"v1.2.0","attachments":[{"name":"latest.txt","downloadUrl":"a"}]},
                {"tagName":"v1.10.0","attachments":[{"name":"App.zip","downloadUrl":"b"}]},
                {"tagName":"v2.0.0","isDraft":true},
                {"tagName":"v3.0.0-rc.1","isPrerelease":true}]}"#,
        )
        .unwrap();
        let latest = latest_release(list.data).unwrap();
        assert_eq!(latest.tag_name, "v1.10.0");
        assert_eq!(latest.attachment(&config().archive_name()), Some("b"));
    }

    #[test]
    fn github_releases_keep_only_this_app_and_api_urls() {
        let list: Vec<GitHubRelease> = serde_json::from_str(
            r#"[{"tag_name":"App-v1.2.0","assets":[{"name":"App.zip","url":"https://api.github.com/repos/o/releases/releases/assets/1"}]},
                {"tag_name":"Other-v9.0.0","assets":[]},
                {"tag_name":"App-v1.3.0","draft":true}]"#,
        )
        .unwrap();
        let latest = latest_release(github_releases(list, "App")).unwrap();
        assert_eq!(latest.tag_name, "v1.2.0");
        assert_eq!(latest.attachment("App.zip"), Some("https://api.github.com/repos/o/releases/releases/assets/1"));
    }

    #[test]
    fn public_github_links_bypass_the_api() {
        assert_eq!(github_public_url("o/app", None, MANIFEST_ASSET), "https://github.com/o/app/releases/latest/download/latest.txt");
        assert_eq!(github_public_url("o/app", Some("1.2.0"), "App.zip"), "https://github.com/o/app/releases/download/v1.2.0/App.zip");
    }

    #[test]
    fn token_is_not_printed() {
        let source = Source::GitHubPrivate { repo: "o/releases".into(), token: "secret".into(), tag_prefix: "App".into() };
        assert!(!format!("{source:?}").contains("secret"));
    }

    #[test]
    fn unpack_extracts_the_named_exe_only() {
        use std::io::Write as _;
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buffer);
        writer.start_file("App.exe", zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(PAYLOAD).unwrap();
        writer.finish().unwrap();
        let dir = temp_dir("unpack");
        let target = dir.join("App.exe.new");
        unpack(buffer.get_ref(), "App.exe", &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), PAYLOAD);
        assert_eq!(unpack(buffer.get_ref(), "Other.exe", &target).unwrap_err().kind, ErrorKind::Manifest);
        assert_eq!(unpack(b"not a zip", "App.exe", &target).unwrap_err().kind, ErrorKind::Manifest);
    }

    #[test]
    fn install_refuses_unsigned_or_current_release() {
        let mut info = info_from("1.0.0", serde_json::from_str(r#"{"version":"2.0.0"}"#).unwrap(), Some("u")).unwrap();
        assert_eq!(install(&config(), &info, |_| {}).unwrap_err().kind, ErrorKind::NoSignature);
        info.available = false;
        assert_eq!(install(&config(), &info, |_| {}).unwrap_err().kind, ErrorKind::UpToDate);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("selfupdate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn swap_puts_new_exe_in_place_and_keeps_old() {
        let dir = temp_dir("swap");
        let paths = Paths::for_exe(&dir.join("App.exe"));
        fs::write(&paths.current, b"old").unwrap();
        fs::write(&paths.new, b"new").unwrap();
        fs::write(&paths.old, b"stale").unwrap();
        swap(&paths).unwrap();
        assert_eq!(fs::read(&paths.current).unwrap(), b"new");
        assert_eq!(fs::read(&paths.old).unwrap(), b"old");
        assert!(!paths.new.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn swap_rolls_back_when_new_exe_is_missing() {
        let dir = temp_dir("rollback");
        let paths = Paths::for_exe(&dir.join("App.exe"));
        fs::write(&paths.current, b"old").unwrap();
        let error = swap(&paths).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Io);
        assert_eq!(fs::read(&paths.current).unwrap(), b"old");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relaunch_target_drops_old_suffix() {
        assert_eq!(original_name(Path::new("C:/apps/App.exe.old")), "App.exe");
        assert_eq!(original_name(Path::new("C:/apps/App.exe")), "App.exe");
    }
}
