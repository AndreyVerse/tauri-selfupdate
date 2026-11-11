#!/usr/bin/env node
// Упаковывает exe релиза в zip, подписывает архив minisign-ключом и пишет рядом latest.txt
// для tauri-selfupdate. С --publish создаёт релиз v<версия> на GitVerse и загружает оба файла.
// Запуск из папки приложения (нужен его @tauri-apps/cli):
//   selfupdate-release --exe release/App.exe --repo owner/App [--version 1.2.3] [--asset App.exe]
//                      [--notes "текст"] [--key путь/к/ключу] [--pub src-tauri/updater.pub] [--publish]
// Версия по умолчанию — из package.json в текущей папке.
// Ключ: --key, иначе TAURI_SIGNING_PRIVATE_KEY_PATH, иначе %USERPROFILE%\.tauri\owner-apps-updater.key.
// --pub (по умолчанию src-tauri/updater.pub, если он есть) — ключ, вшитый в приложение:
// подпись чужим ключом приложение отвергнет, поэтому такой манифест не пишется.
// --publish берёт токен из GITVERSE_TOKEN; токен в аргументы не передаётся, чтобы не попасть в историю shell.
// --github owner/releases вместо GitVerse публикует через gh в общий приватный репозиторий
// релизов: тег <имя exe без .exe>-v<версия>.
// --github-public публикует через gh в открытый репозиторий приложения (--repo): тег v<версия>;
// если релиз с этим тегом уже есть, дозагружает архив и манифест, но не заменяет их.
import { execFileSync, execSync, spawnSync } from 'node:child_process';
import {
  copyFileSync,
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const index = args.indexOf(name);
  return index === -1 ? fallback : args[index + 1];
};

const exe = arg('--exe');
const version = (
  arg('--version') ?? JSON.parse(readFileSync('package.json', 'utf8')).version
)?.replace(/^v/, '');
const repo = arg('--repo');
if (!exe || !version || !repo) {
  console.error(
    'usage: selfupdate-release --exe <path> --repo <owner/name> [--version <x.y.z>] [--asset <name>] [--notes <text>] [--key <path>] [--publish]',
  );
  process.exit(2);
}
const asset = arg('--asset', basename(exe));
const notes = arg('--notes', '');
const publish = args.includes('--publish');
const github = arg('--github');
const githubPublic = args.includes('--github-public');
const key = resolve(
  arg(
    '--key',
    process.env.TAURI_SIGNING_PRIVATE_KEY_PATH ??
      join(homedir(), '.tauri', 'owner-apps-updater.key'),
  ),
);
if (!existsSync(exe)) throw new Error(`нет файла ${exe}`);
if (!existsSync(key)) throw new Error(`нет ключа подписи ${key}`);
const token = process.env.GITVERSE_TOKEN?.trim();
if (publish && !github && !githubPublic && !token)
  throw new Error('для --publish нужен GITVERSE_TOKEN');

// GitVerse не принимает .exe во вложения; имя внутри архива — то, что ищет приложение.
const out = dirname(resolve(exe));
const archive = join(out, `${asset.replace(/\.exe$/i, '')}.zip`);
const staging = mkdtempSync(join(tmpdir(), 'selfupdate-'));
try {
  copyFileSync(exe, join(staging, asset));
  rmSync(archive, { force: true });
  execFileSync(
    'powershell',
    [
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      `Compress-Archive -LiteralPath '${join(staging, asset).replaceAll("'", "''")}' -DestinationPath '${archive.replaceAll("'", "''")}' -CompressionLevel Optimal`,
    ],
    { stdio: 'inherit' },
  );
} finally {
  rmSync(staging, { recursive: true, force: true });
}

const signatureFile = `${archive}.sig`;
rmSync(signatureFile, { force: true });
// Пароль пустой — так ключ создан `tauri signer generate -p ""`; другой задаётся окружением.
const password = process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? '';
// npx на Windows — это .cmd и запускается только через shell: команда одной строкой,
// пустой пароль — `--password=` одним словом, пути — в кавычках (могут быть с пробелами).
const quote = (value) => `"${value.replaceAll('"', '\\"')}"`;
execSync(
  `npx tauri signer sign -f ${quote(key)} ${quote(`--password=${password}`)} ${quote(archive)}`,
  { stdio: 'inherit' },
);
const signature = readFileSync(signatureFile, 'utf8').trim();
rmSync(signatureFile, { force: true });

// Формат tauri signer: base64 поверх minisign-текста; во второй строке байты 2..10 — key id.
const keyId = (wrapped) => {
  const line = Buffer.from(wrapped.trim(), 'base64').toString('utf8').split(/\r?\n/)[1] ?? '';
  return Buffer.from(line, 'base64').subarray(2, 10).toString('hex');
};
const pub = arg('--pub', existsSync('src-tauri/updater.pub') ? 'src-tauri/updater.pub' : undefined);
if (pub) {
  const expected = keyId(readFileSync(pub, 'utf8'));
  const actual = keyId(signature);
  if (!expected || expected !== actual) {
    throw new Error(
      `подпись ключом ${actual}, а приложение ждёт ${expected} (${pub}): обновление было бы отвергнуто`,
    );
  }
}

// Адреса архива в манифесте нет: приложение берёт его из того же релиза.
const manifest = { version, notes, pub_date: new Date().toISOString(), exe: { signature } };
const manifestFile = join(out, 'latest.txt');
writeFileSync(manifestFile, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`wrote ${archive}\nwrote ${manifestFile}`);

if (publish && githubPublic) {
  const tag = `v${version}`;
  const view = spawnSync('gh', ['release', 'view', tag, '-R', repo, '--json', 'assets'], {
    encoding: 'utf8',
  });
  if (view.status === 0) {
    const present = new Set(JSON.parse(view.stdout).assets.map((item) => item.name));
    for (const file of [archive, manifestFile]) {
      // Замена вложения сломала бы подпись у тех, кто уже качает: переиздают новой версией.
      if (present.has(basename(file)))
        throw new Error(`в ${repo} ${tag} уже есть ${basename(file)}: выпустите новую версию`);
    }
    execFileSync('gh', ['release', 'upload', tag, archive, manifestFile, '-R', repo], {
      stdio: 'inherit',
    });
  } else {
    execFileSync(
      'gh',
      [
        'release',
        'create',
        tag,
        archive,
        manifestFile,
        '-R',
        repo,
        '--title',
        `${basename(asset, '.exe')} ${version}`,
        '--notes',
        notes || tag,
      ],
      { stdio: 'inherit' },
    );
  }
} else if (publish && github) {
  const tag = `${asset.replace(/\.exe$/i, '')}-v${version}`;
  // Уже выпущенный релиз переиздают новой версией: замена вложения сломала бы подпись у тех, кто качает.
  const exists =
    spawnSync('gh', ['release', 'view', tag, '-R', github], { stdio: 'ignore' }).status === 0;
  if (exists) throw new Error(`в ${github} уже есть ${tag}: выпустите новую версию`);
  execFileSync(
    'gh',
    [
      'release',
      'create',
      tag,
      archive,
      manifestFile,
      '-R',
      github,
      '--title',
      `${basename(asset, '.exe')} ${version}`,
      '--notes',
      notes || tag,
    ],
    { stdio: 'inherit' },
  );
} else if (publish) {
  const api = `https://api.gitverse.ru/repos/${repo}`;
  const headers = {
    Authorization: `Bearer ${token}`,
    Accept: 'application/vnd.gitverse.object+json;version=latest',
  };
  const call = async (method, path, body) => {
    // FormData задаёт свой multipart-заголовок; JSON GitVerse без Content-Type отвергает (422).
    const json = typeof body === 'string' ? { 'Content-Type': 'application/json' } : {};
    const response = await fetch(`${api}${path}`, {
      method,
      headers: { ...headers, ...json },
      body,
    });
    const text = await response.text();
    if (!response.ok)
      throw new Error(`${method} ${path}: HTTP ${response.status} ${text.slice(0, 300)}`);
    return JSON.parse(text);
  };
  const tag = `v${version}`;
  const existing = await fetch(`${api}/releases/tags/${tag}`, { headers });
  const release = existing.ok
    ? await existing.json()
    : await call(
        'POST',
        '/releases',
        JSON.stringify({
          tag_name: tag,
          name: `${basename(asset, '.exe')} ${version}`,
          body: notes,
          target_commitish: 'main',
        }),
      );
  const present = new Set((release.assets ?? []).map((item) => item.name));
  for (const file of [archive, manifestFile]) {
    const name = basename(file);
    // Замена вложения меняет его адрес; уже выпущенный релиз переиздают новой версией.
    if (present.has(name)) throw new Error(`в ${tag} уже есть ${name}: выпустите новую версию`);
    const form = new FormData();
    form.append('attachment', new Blob([readFileSync(file)]), name);
    const uploaded = await call(
      'POST',
      `/releases/${release.id}/assets?name=${encodeURIComponent(name)}`,
      form,
    );
    console.log(`uploaded ${name}: ${uploaded.browser_download_url}`);
  }
}
