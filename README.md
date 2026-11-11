<div align="center">

# tauri-selfupdate

**Обновление портативных Windows-приложений на Tauri в один клик: без установщика, сервера обновлений и прав администратора.**

[![Rust](https://img.shields.io/badge/Rust-crate-B7410E?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white)](https://tauri.app/)
[![Лицензия: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-c9a860.svg)](LICENSE)

</div>

---

> Это библиотека, готового exe у неё нет: она подключается к приложению как зависимость. Релизы берутся из открытого репозитория GitHub, публичного репозитория GitVerse или приватного репозитория GitHub по токену только на чтение.

## ⭐ Чем лучше `tauri-plugin-updater`

- **Портативный exe, а не установщик.** Новый exe скачивается рядом с работающим и подменяется переименованием. Ярлык, папка и данные пользователя остаются на месте.
- **Релизы — весь бэкенд.** Открытый репозиторий GitHub или GitVerse без токена, либо приватный репозиторий GitHub, общий для нескольких программ: так обновления получают только те, у кого есть exe. Выпуск обновления — одна команда.
- **Подпись обязательна.** Каждый архив с exe сверяется с подписью minisign из манифеста. Манифест без подписи отвергается, файл с неверной подписью удаляется.
- **Безопасная подмена с откатом.** Проверка размера, `fsync` перед переименованием, возврат старого exe, если новый не удалось поставить. На процесс — только одна установка за раз.
- **Перезапуск, совместимый с single-instance.** Новый процесс ждёт настоящего завершения старого и только потом занимает блокировку единственного экземпляра.
- **Маленькая и синхронная.** `ureq` и `minisign-verify`, асинхронная среда не нужна. Вызывайте через `spawn_blocking`.

## Подключение

```toml
[dependencies]
tauri-selfupdate = { git = "https://github.com/AndreyVerse/tauri-selfupdate", branch = "main" }
```

```rust
fn main() {
    tauri_selfupdate::startup(); // первая строка: убирает следы и ждёт завершения прошлого процесса
    // ... tauri::Builder ...
}

let config = tauri_selfupdate::Config {
    // Без токена при сборке — открытый GitHub (`Source::GitHub`) или GitVerse
    // (`Source::GitVerse`), с токеном — приватный GitHub.
    source: match option_env!("SELFUPDATE_GITHUB_TOKEN") {
        Some(token) => tauri_selfupdate::Source::GitHubPrivate {
            repo: "owner/releases".into(),
            token: token.into(),
            tag_prefix: "App".into(),
        },
        None => tauri_selfupdate::Source::GitHub { repo: "owner/App".into() },
    },
    asset: "App.exe".into(),
    public_key: include_str!("../updater.pub").trim().into(),
    user_agent: "App".into(),
};
let info = tauri_selfupdate::check(&config, env!("CARGO_PKG_VERSION"))?;
if info.available {
    tauri_selfupdate::install(&config, &info, |p| println!("{}/{}", p.downloaded, p.total))?;
    tauri_selfupdate::relaunch()?;
    app.exit(0);
}
```

У ошибок есть стабильное поле `kind` (`network`, `http`, `manifest`, `noSignature`, `signature`, `incomplete`, `io`, `upToDate`, `busy`) для локализованных сообщений, и они сразу сериализуются в результат команды Tauri.

## Выпуск обновления

```bash
npx tauri signer generate -w ~/.tauri/app-updater.key -p "" --ci   # один раз; приватный ключ не кладите в git
npm i -D git+https://github.com/AndreyVerse/tauri-selfupdate.git
npx selfupdate-release --exe release/App.exe --repo owner/App --key ~/.tauri/app-updater.key --publish   # GitVerse, версия из package.json
npx selfupdate-release --exe release/App.exe --repo owner/App --github-public --publish                # открытый GitHub через gh
npx selfupdate-release --exe release/App.exe --repo owner/App --github owner/releases --publish        # приватный GitHub через gh
```

Скрипт упакует `App.exe` в `App.zip` (GitVerse не принимает `.exe` во вложения), подпишет архив, запишет манифест `latest.txt` и с флагом `--publish` создаст релиз `v1.2.3` и загрузит оба файла; с `--github-public` — релиз `v1.2.3` в открытом репозитории приложения через `gh` (у уже созданного релиза дозагружает недостающие файлы); с `--github` — релиз `App-v1.2.3` в общем репозитории через `gh`. Токен берётся из переменной `GITVERSE_TOKEN`. Без `--publish` файлы остаются рядом с exe, загрузить их можно вручную. Публичный ключ (`app-updater.key.pub`) вкомпилируется в приложение.

Чтобы отрепетировать обновление локально, укажите в `SELFUPDATE_MANIFEST_URL` манифест на своём сервере с полем `exe.url`: подпись всё равно сверяется с вшитым ключом.

## Лицензия

[Apache License 2.0](LICENSE).

## Приватные релизы на GitHub

Токен создаётся как fine-grained: доступ только к репозиторию релизов, право Contents — Read-only. Он вшивается при сборке переменной `SELFUPDATE_GITHUB_TOKEN` и уходит только на `api.github.com`: вложение GitHub отдаёт редиректом на подписанный адрес, и токен туда не передаётся. Кто получил exe, может достать из него токен, но откроет им только сборки. Срок токена ограничен: выпустите версию с новым токеном до истечения старого, иначе установленные копии перестанут видеть обновления.
