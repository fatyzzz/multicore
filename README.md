# MultiCore

Нативный Windows-клиент на Rust без WebView. Внутри работают два независимых ядра:

- Mihomo владеет TUN, DNS, sniffing, правилами по процессам, группами и селекторами;
- Xray обслуживает готовые Shadowsocks + FinalMask outbounds через локальные SOCKS inbounds;
- Rust daemon атомарно обновляет конфигурации и запускает Xray перед Mihomo;
- Slint desktop оставляет пользователю один основной сценарий: добавить подписку, подключиться, выбрать маршрут и отключиться.

## Запуск готового клиента

Откройте:

```text
dist\multicore-windows-x64\MultiCore.exe
```

Это единственный пользовательский `.exe`. Он сам:

1. запрашивает стандартное подтверждение Windows UAC, затем проверяет комплектность `runtime\multicore-daemon.exe` и обоих ядер;
2. создаёт случайный локальный токен;
3. без консоли запускает daemon на свободном loopback-порту;
4. проверяет авторизованный `/v1/status`;
5. при закрытии уничтожает принадлежащее ему дерево процессов через Windows Job Object.

Никакие переменные окружения и второе окно PowerShell для обычного запуска не нужны. Пользовательские данные хранятся в `%LOCALAPPDATA%\MultiCore`.

В portable preview зафиксированы проверенные upstream-сборки на 15 сентября 2026 года:

- Xray-core `v26.9.9`, commit `52a412d9e2f5c2a5142b1b4e2ab3771dacb8b120`;
- Mihomo `v1.19.30`, commit `ac017cdd246ce8bd547653d927e7bf77d7ee73d5`.

Точные upstream URL и SHA-256 архивов/исполняемых файлов записаны в `versions.json`, итоговые хеши пакета — в `SHA256SUMS.txt`.

## Формат подписки

Клиент использует один URL и три фиксированных User-Agent:

- `multicore-json-massive` — JSON tuple `["<Mihomo YAML>", {<Xray JSON>}]`;
- `multicore-mihomo` — полный сырой Mihomo YAML;
- `multicore-xray` — один готовый Xray JSON object.

Сначала выполняется один запрос combined tuple. При успешном HTTP-ответе другие запросы не выполняются; битый tuple возвращает ошибку. Только если massive-вариант недоступен или не поддерживается сервером, две части загружаются отдельно. Только Mihomo YAML разбирается для групп и UI. Полученный Xray JSON проверяется на UTF-8/JSON object и хранится byte-exact. Непосредственно перед Connect клиент заново определяет активный Windows default interface, локально разрешает только домены Xray-серверов и создаёт отдельную ephemeral runtime-копию: merge `dns.hosts`, `sockopt.domainStrategy: ForceIP` и `sockopt.interface`. FinalMask и все остальные серверные поля сохраняются.

Контракт для backend: [docs/contracts/multicore-subscription-backend-prompt.md](docs/contracts/multicore-subscription-backend-prompt.md).

## Сборка из исходников

Нужны Rust/Cargo ровно 1.98.1 и MSVC Build Tools:

```powershell
$env:RUSTUP_TOOLCHAIN = '1.98.1-x86_64-pc-windows-msvc'
& "$env:USERPROFILE\.cargo\bin\cargo.exe" build --workspace --release --locked
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test --workspace --all-targets
& "$env:USERPROFILE\.cargo\bin\cargo.exe" clippy --workspace --all-targets -- -D warnings
```

Собрать portable preview с pinned-ядрами:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\package-windows-release.ps1 `
  -DestinationPath dist\multicore-windows-x64
```

Сборка с закреплённым GitHub Releases-каналом и готовым asset для автообновления:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\package-windows-release.ps1 `
  -DestinationPath dist\multicore-windows-x64 `
  -UpdateRepository owner/repository `
  -ReleaseAssetPath dist\release\multicore-windows-x64.zip
```

Репозиторий вшивается при сборке и не меняется из UI. Клиент проверяет только
последний стабильный GitHub Release с тегом `vX.Y.Z` и asset
`multicore-windows-x64.zip`. Перед заменой всего portable-пакета проверяются
размер, GitHub SHA-256 digest, безопасные пути архива и внутренний
`SHA256SUMS.txt`; установка заблокирована, пока VPN подключён. Старый каталог
сохраняется рядом для отката. Канал пока не Authenticode/offline-key signed:
перед публичной раздачей подпись и защита release-аккаунта остаются обязательны.

Production-упаковщик сам собирает оба Rust-бинарника, проверяет версии Cargo/rustc 1.98.1, удаляет приватные пути сборщика и не принимает готовые `.exe`. Он принимает online только committed manifest `packaging\windows-x64\versions.json`, проверяет точные официальные URL, SHA-256, ZIP entry и PE32+ AMD64. Существующий destination он не перезаписывает.

Проверки упаковки и запуска:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-package-windows-release.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-windows-installer-contract.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\test-release-workflow-contract.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\smoke-test-one-click.ps1
```

Собрать per-user установщик из уже проверенного portable-каталога:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\build-windows-installer.ps1 `
  -PackagePath dist\multicore-windows-x64 `
  -OutputDirectory dist\release `
  -AppVersion 0.1.2
```

Установщик кладёт клиент в `%LOCALAPPDATA%\Programs\MultiCore`, добавляет ярлык,
опциональный фоновый автозапуск и регистрирует `multicore://install-sub?url=…`.
Тег `vX.Y.Z`, совпадающий с версией desktop crate, запускает Windows release
workflow: он повторно проверяет код, собирает portable ZIP и installer, прикладывает
точные upstream source archives и только затем публикует GitHub Release.

## Ограничения релиза

Бинарники пока не имеют Authenticode-подписи, поэтому Windows SmartScreen может
показать предупреждение. TUN elevation остаётся runtime-задачей; установщик намеренно
per-user и не маскирует её. Точные лицензии и notices находятся в
`THIRD_PARTY_NOTICES.md` внутри пакета, а release workflow сохраняет immutable
corresponding source для зафиксированных версий ядер.
