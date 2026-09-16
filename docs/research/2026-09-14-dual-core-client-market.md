# Рынок desktop-клиентов и стратегия двухъядерного клиента Xray + Mihomo

## Резюме

Проект имеет смысл, но не как «ещё один GUI для прокси». Его сильная позиция — **Mihomo как control plane, Xray как transport plane**:

- Mihomo единолично владеет TUN, DNS, sniffing, process routing, rule providers, selector/url-test/fallback/load-balance и активными соединениями.
- Xray не принимает глобальные решения. Он получает уже выбранный поток через локальный SOCKS-вход и реализует конкретный outbound, включая Xray-специфичные возможности вроде FinalMask.
- Rust-приложение является supervisor: импортирует подписки, строит обе runtime-конфигурации, запускает ядра, синхронизирует их, проверяет состояние и восстанавливает сеть после сбоя.

Это оправдано именно наличием функционального разрыва. В Xray есть реализация FinalMask на уровне transport stack, включая TCP/UDP masks и набор реализаций fragment, noise, xdns, xicmp, sudoku и других.[^1] Mihomo при этом значительно сильнее как готовая политика маршрутизации: process/path rules, логические правила, rule providers, policy groups, health checks и полноценный TUN/DNS-контур.[^2] [^3] Xray уже получил собственный TUN и process matching, но его документация и свежие issue показывают молодость реализации: риски loop, DNS/IPv6 и выбора неправильного интерфейса всё ещё приходится учитывать.[^4] [^5]

Главный рыночный вывод: готовых клиентов много, но почти все выбирают одно активное ядро или используют второе как отдельный режим. Понятного продукта, где решение Mihomo прозрачно продолжается Xray-outbound и пользователь видит единую трассу, практически нет. Это и есть окно для продукта.

Рекомендация по UI: **Rust daemon/supervisor + Tauri 2 для первого релиза**. Это всё ещё WebView, но не Electron: Tauri использует системный WebView, не поставляет собственный browser runtime и оставляет системную часть в Rust.[^6] Если отсутствие веб-стека принципиально, лучший кандидат — Slint; чистый Rust GUI стоит рассматривать как второй frontend поверх уже стабильного daemon API, а не связывать с ним жизнеспособность всей архитектуры.

## Проверка исходной идеи

| Вопрос | Вывод |
|---|---|
| Проблема действительно существует? | Да. Mihomo покрывает большинство обычных протоколов, но не закрывает Xray-специфичный транспортный хвост. Xray, в свою очередь, не является столь зрелой desktop-системой TUN/DNS/process policy. |
| Два ядра пропорциональны проблеме? | Да, если роли жёстко разделены и связь идёт через стабильные локальные протоколы/API. Нет, если оба ядра одновременно управляют TUN, DNS или routing. |
| Цена отказа от проекта | Пользователь продолжит переключать ядра/клиенты и терять либо Xray-only transport capabilities, либо удобный policy layer. Для обычного VLESS/Reality пользователя цена мала; для FinalMask-аудитории — существенна. |

Следовательно, целевая аудитория не «все пользователи VPN», а технически грамотные пользователи и провайдеры конфигураций, которым одновременно нужны новые Xray transports и Clash/Mihomo-style routing. Простота Hiddify должна быть режимом интерфейса, а не попыткой скрыть саму сложность продукта.

## Карта рынка

### Основные desktop-конкуренты

| Клиент | Ядра и стек UI | Сильные стороны | Что стоит перенять / чего избегать |
|---|---|---|---|
| **v2rayN** | Xray, sing-box и custom cores; WPF на Windows, Avalonia-сборка для desktop | Самая широкая Xray-совместимость, множество форматов, цепочки, policy groups, TUN, глубокие настройки. Проект уже применяет дополнительный front service для routing/traffic display перед custom core.[^7] [^8] | Перенять широту импорта, capability-aware UI и идею front service. Не копировать перегруженную модель «настройки ядра определяют весь интерфейс» и не изменять пользовательский raw config без наглядного diff. |
| **Clash Verge Rev** | Mihomo; Tauri 2 + Rust + web frontend | Сильный desktop benchmark: TUN, system proxy/guard, Merge/Script overrides, visual node/rule editors, syntax hints, tray, WebDAV, stable/autobuild channels.[^9] | Перенять layered profiles, service/sidecar режимы, tray workflow и безопасный rollback. Улучшить объяснимость routing и не превращать CSS/JS injection в центральную функцию. |
| **Clash Nyanpasu** | Mihomo, Clash Premium, Clash Rust, Meow; Tauri + React | Multi-core shell, provider management, enhancement через YAML/JavaScript/Lua, Material You.[^10] | Перенять единый интерфейс к нескольким core adapters. Не позволять arbitrary scripting стать обязательным способом настроить базовое поведение. |
| **Clash Party / Sparkle** | Mihomo/Smart core; Electron + React | Service-free TUN, мощные overrides, Sub-Store, WebDAV, темы, редактирование большинства Mihomo-настроек.[^11] | Перенять subscription pipeline и backup. Не переносить Electron, рекламную интеграцию и непрозрачное «AI smart selection» без объяснимого score. |
| **FlClash** | Mihomo; Flutter | Один UI для Android/Windows/macOS/Linux, adaptive layout, Material You, WebDAV, простой onboarding.[^12] | Перенять адаптивность и быстрый основной сценарий. Desktop UI не должен ощущаться растянутым мобильным экраном. |
| **Hiddify** | sing-box; Flutter | Лучший ориентир для новичка: remote profiles, auto-selection, импорт Sing-box/V2Ray/Clash, автообновление, показ остатка трафика и срока подписки, официальные stores.[^13] | Перенять подключение за 1–2 действия, usage/expiry и понятные ошибки. Advanced mode должен оставаться доступным. |
| **Throne** | sing-box + Xray/custom core; Qt/C++ | Нативный desktop UI, chaining, custom outbounds/configs, extra core, deeplinks и широкий набор протоколов.[^14] | Перенять deeplinks, custom core contract и цепочки. Отдельно решить signing/elevation: текущая macOS-инструкция показывает, насколько ломким бывает ручной privilege flow. |
| **GUI.for.Clash** | Mihomo; Wails + Vue | Малый desktop shell, плагинный центр, отдельные core-продукты и общий plugin hub.[^15] [^16] | Перенять узкий plugin manifest и permissions. Не запускать непроверенный JS с доступом к системе/секретам. |
| **Invisible Man XRay** | Xray; .NET + Go wrapper | Фокусированный Xray-клиент с простым переключением серверов и отдельным TUN service.[^17] | Перенять простоту single-purpose UI. Его функциональный потолок показывает, почему одного Xray GUI недостаточно для задуманного продукта. |
| **Furious** | Xray + Hysteria; PySide6/Qt | Cross-platform, TUN, subscription scheduler, встроенный config editor.[^18] | Перенять редактируемость и portable mindset; не смешивать все настройки в один raw editor. |
| **XRAT** | Xray/V2Ray/sing-box; Rust CLI/TUI + daemon | Самый полезный Rust-референс: daemon живёт отдельно от UI, real HTTP latency, сохранённые результаты, причины отказов, rotation, QR, structured export, SQLite и HTTP API.[^19] | Перенять daemon-first архитектуру, измерения полного пути и CLI как first-class frontend. Это близкий архитектурный ориентир, но не прямой GUI-конкурент. |

### Что рынок уже считает обязательным

У зрелых клиентов повторяются одни и те же блоки:

1. Импорт URL, raw links, clipboard, QR и deeplink.
2. Профили/подписки с автообновлением и информацией о quota/expiry.
3. Режимы Off / System Proxy / TUN и корректное восстановление системных настроек.
4. Selector, URL-test, fallback, load-balance и health checks.
5. Connections, traffic graph, logs и принудительное закрытие соединений.
6. Rule providers, GeoIP/GeoSite, overrides/mixins и raw editor.
7. Tray, autostart, auto-connect, hotkeys, dark mode и локализация.
8. Core/data updater, backup/sync и экспорт диагностического пакета.

Просто реализовать этот список недостаточно: получится функциональный клон Clash Verge Rev или v2rayN. Отличие должно находиться в **единой наблюдаемой трассе двух ядер** и в надёжности системного networking lifecycle.

## Рекомендуемая архитектура

```text
Applications
    │
    ▼
Mihomo TUN
  DNS / sniffing / PROCESS-* / rules / rule-providers
  select / url-test / fallback / load-balance
    │
    ├── DIRECT / REJECT ───────────────────────────────► network / drop
    │
    └── XRAY::<node-id> (local SOCKS5, TCP+UDP)
                │
                ▼
       Xray SOCKS inbound tagged node-id
                │  inboundTag -> outboundTag
                ▼
       Xray outbound (VLESS/XHTTP/REALITY/FinalMask/...)
                │
                ▼
              network

Rust supervisor
  profile compiler · port allocator · core lifecycle · APIs · state recovery
```

### Почему TUN должен принадлежать Mihomo

Mihomo TUN поддерживает system/gVisor/mixed stacks, automatic routing, DNS hijack и platform-specific include/exclude controls; его routing умеет process name/path/regex, UID, network, inbound и logical rules.[^2] [^20] Его DNS-модуль содержит fake-IP/redir-host, отдельные resolvers для proxy servers и direct traffic, nameserver policy, fallback и rule-aware DNS.[^3]

Xray TUN существует и поддерживает Windows/Linux/macOS/FreeBSD, а Xray routing уже умеет process matching.[^4] [^21] Но это не отменяет архитектурного решения: два владельца системных routes и DNS создадут гонки, loops и неотлаживаемые сбои. Свежие Xray issue документируют high-load instability, неверный выбор Hyper-V/WSL interface и случаи DNS/IPv6/loop problems.[^5] [^22]

Если под «demodoor» подразумевался `dokodemo-door`, важно не смешивать термины: сейчас он называется `Tunnel` и является port-mapping inbound, а не TUN-адаптером.[^23]

### Как представить Xray-ноды селекторам Mihomo

Самый прямой MVP:

- Один процесс Xray.
- Для каждой доступной Xray-ноды — отдельный локальный SOCKS inbound на loopback с уникальным tag/port.
- В Xray rule `inboundTag -> outboundTag` жёстко связывает этот вход с конкретным outbound.
- Rust compiler генерирует в Mihomo соответствующий локальный `type: socks5`, `udp: true` и добавляет его в нужные proxy groups.
- Mihomo health check проходит через весь путь и измеряет не локальный SOCKS handshake, а реальную доступность exit.

Xray SOCKS inbound поддерживает UDP, а Mihomo SOCKS outbound имеет `udp: true`, поэтому TCP и UDP можно провести без второго TUN.[^24] [^25] Xray HandlerService умеет runtime add/remove inbound и outbound, поэтому обновление подписки не обязано перезапускать весь core.[^26]

Ограничение: схема «порт на каждую ноду» плохо масштабируется на подписки с сотнями или тысячами записей. Для первого релиза разумен configurable cap и lazy materialization: активная нода, кандидаты health-check и ближайший резерв. После проверки спроса можно добавить Rust dispatch bridge или пул Xray slots. Не следует начинать с собственного SOCKS multiplexer: он увеличит объём критичного networking-кода до того, как доказана ценность продукта.

### Обязательные инварианты

- Только Mihomo меняет system routes, TUN и system DNS.
- Xray слушает только loopback; его API также только loopback/named pipe/Unix socket.
- Xray outbound socket принудительно привязывается к физическому интерфейсу, чтобы Mihomo TUN не захватил его повторно.
- Domain ownership остаётся у Mihomo; Xray получает исходное имя через SOCKS и использует `targetStrategy: AsIs`, если профиль не требует иного.
- UDP включается только когда его поддерживают оба локальных hop и конечный transport; UI показывает capability, а не молча деградирует.
- Core crash не оставляет испорченные routes/proxy settings. Supervisor хранит last-known-good state и выполняет idempotent cleanup.
- Любая конфигурация проходит `mihomo -t` и Xray test до атомарного переключения.
- В production не должно быть второго скрытого routing layer внутри Xray, кроме технических правил `inboundTag -> outboundTag` и защиты core traffic.

## UI: нативный Rust или WebView

| Вариант | Оценка | Решение |
|---|---|---|
| **Tauri 2 + React/Svelte** | Системный WebView, Rust backend, зрелые tables/editors/charts/i18n, tray/updater/IPC. Основные Mihomo desktop-клиенты уже доказали этот путь.[^6] [^9] | **Рекомендуется для v1.** UI не должен содержать networking logic; он общается с daemon API. |
| **Slint** | Компилируемый declarative UI, официальный Rust API, Windows/macOS/Linux. Хорош для settings/dashboard; придётся самим доводить большие таблицы, editor и accessibility. Лицензия требует осознанного выбора GPL, attribution-based community или commercial условий.[^27] | Лучший вариант, если «никакого WebView» — жёсткое требование. Делать после headless PoC. |
| **Iced** | Pure Rust, Elm-подобная архитектура, активно развивается. Больше UI-инфраструктуры и platform polish придётся собирать самостоятельно.[^28] | Подходит команде, готовой инвестировать в собственную design system. Не даёт продуктового преимущества сам по себе. |
| **egui** | Быстрый pure-Rust immediate mode. Авторы прямо не ставят целью native look и отмечают меняющиеся API/неполноту framework-функций.[^29] | Отличен для internal inspector/diagnostics; слабее как основной polished consumer UI. |

Практичный компромисс: выпустить один `core-daemon` на Rust, CLI и Tauri UI. Если WebView окажется реальной причиной расхода памяти, blank screen или недоверия аудитории, Slint frontend можно добавить без переписывания orchestration layer. Если начать со Slint монолитом, UI-риск смешается с самым сложным местом проекта — lifecycle двух proxy cores.

## Функции, которые дадут продукту лицо

### P0 — без этого нельзя выпускать

1. **Транзакционный Connect.** Compile → validate → snapshot OS state → start cores → health probe → enable system capture. При ошибке полный rollback.
2. **Единый selector.** Пользователь видит группу Mihomo, но каждая карточка показывает конечный Xray transport, FinalMask, UDP-capability и фактический exit.
3. **Route Explain.** Для выбранного соединения: process → domain/IP → DNS decision → matched rule/provider → selector → Xray inbound/outbound → exit.
4. **Live Connections.** Process, destination, protocol, matched rule, policy group, Xray node, upload/download, duration и Kill.
5. **Честный тест ноды.** Раздельно TCP connect, proxy handshake, TLS/transport, HTTP TTFB, throughput и failure reason. Ping в одиночку не должен называться скоростью.
6. **DNS Guard.** Leak test, текущие resolvers, fake-IP mapping viewer, IPv4/IPv6 диагностика и предупреждение о конфликтующем system resolver.
7. **Layered profiles.** `subscription (immutable) + user patch + generated bridge + runtime config`; перед применением показывать diff и источник каждого поля.
8. **Core manager.** Pinned stable/canary versions, hashes/signatures, staged update, compatibility matrix и one-click rollback.
9. **Headless daemon + CLI.** GUI можно закрыть, соединение продолжает работать. CLI покрывает import/connect/status/diagnose/export.
10. **Безопасный support bundle.** Версии, sanitized configs, route table, interfaces и последние события; UUID, URL подписок, auth, SNI и IP пользователя редактируются по умолчанию.

### P1 — сильные дифференциаторы

- **Policy Studio:** визуальные правила по приложению, домену, сети и SSID с live simulation до применения.
- **Connection replay:** взять metadata закрытого соединения и показать, как его направит новая конфигурация.
- **Sticky smart failover:** переключение по loss/TTFB/ошибкам с hysteresis и cooldown, а не постоянная гонка за минимальным ping.
- **Network contexts:** разные профили для домашнего Wi‑Fi, мобильного hotspot, Ethernet, captive portal и IPv6-only сети.
- **Subscription hygiene:** dedup по canonical node fingerprint, конфликт имён, устаревшие/небезопасные параметры, quarantine вместо тихого удаления.
- **Capability negotiation:** UI заранее сообщает, почему конкретная нода не поддерживает UDP/FinalMask/выбранный режим.
- **Timeline двух ядер:** единый event stream с correlation ID; не два несвязанных окна логов.
- **Crash lab:** кнопка самопроверки recovery — контролируемо завершить sidecar и убедиться, что сеть восстановлена.

### P2 — только после стабильного ядра продукта

- WebDAV/локальный encrypted backup.
- Plugin system с декларативными permissions и подписанным каталогом.
- Mobile frontend.
- Полный visual editor каждого поля Xray/Mihomo.
- Remote fleet/organization features.

## Чего не стоит делать

- Не давать пользователю выбор «кто сегодня владеет TUN — Xray или Mihomo» в обычном UI. Это диагностический режим, не продуктовая функция.
- Не копировать arbitrary JS/Lua overrides в MVP. Сначала typed patches, schema validation и diff.
- Не обещать «AI выберет лучший сервер», пока score нельзя разложить на измеряемые компоненты.
- Не делать raw YAML/JSON источником истины приложения. Хранить нормализованную domain model и генерировать runtime artifacts.
- Не скрывать принудительную смену core, DNS или порта. Подобное поведение уже вызывает вопросы у пользователей v2rayN.[^30]
- Не писать собственный TUN stack, DNS resolver или transport core.
- Не начинать одновременно с Windows, macOS, Linux и mobile. Наиболее рациональный первый target — Windows 10/11 x64/arm64, затем Linux; macOS требует отдельной работы с signing, notarization и privileged helper.

## Безопасность, лицензии и поставка

Xray-core распространяется под MPL-2.0; copyleft применяется на уровне файлов и допускает larger work с другими лицензиями.[^31] Mihomo core распространяется под GPL-3.0 и отдельно просит downstream-проекты вне MetaCubeX не использовать слово `mihomo` в названии.[^32]

Самый простой и низкорисковый путь — открыть клиент под GPL-3.0-compatible лицензией и публиковать воспроизводимые сборки. Если продукт должен быть proprietary, процессы следует держать отдельными sidecar executables и общаться через SOCKS/API. FSF считает pipes/sockets/command-line типичными механизмами отдельных программ, но подчёркивает, что итог зависит и от семантики связи; при распространении GPL binary всё равно требуется предоставить соответствующий исходный код и условия лицензии.[^33] Это не юридическое заключение — перед коммерческим релизом нужен отдельный license review.

Supply-chain минимум:

- manifest с version, source URL, SHA-256 и signature status каждого core/data bundle;
- обновление через временный файл, проверку подписи/хэша и atomic replace;
- сохранение предыдущей рабочей версии;
- запрет core download с произвольного mirror по умолчанию;
- secret URLs/credentials в OS keychain, не в SQLite/plain logs;
- loopback APIs с random secret; named pipe/Unix socket предпочтительнее TCP controller;
- журнал всех elevated networking changes и гарантированный cleanup.

## Предлагаемое позиционирование

Не продавать идею словами «клиент с двумя ядрами» — пользователь покупает не архитектуру. Позиционирование:

> **Xray transports. Mihomo routing. Один понятный маршрут.**

Ключевое обещание: «подключает Xray-only конфигурации без отказа от нормального split tunneling и показывает, почему каждый процесс пошёл именно этим путём».

У продукта есть три режима сложности:

- **Simple:** вставить подписку, выбрать профиль, подключиться.
- **Rules:** приложения, домены, группы, health/failover.
- **Expert:** layered diff, raw artifacts, core channels, traces и diagnostics.

## Итоговое решение

Строить стоит. Архитектурная формула для первой версии:

1. Windows-first Rust daemon/supervisor.
2. Mihomo владеет TUN/DNS/process routing/selectors.
3. Один Xray process предоставляет локальные tagged SOCKS inbounds и Xray-only outbounds с FinalMask.
4. Связь и наблюдаемость идут через Mihomo REST/WebSocket API, Xray gRPC API и единый event model Rust-приложения.
5. Первый UI — Tauri 2; CLI обязательна. Slint остаётся реальным запасным frontend, а не условием старта.
6. Первый продуктовый spike должен доказать TCP, UDP, DNS preservation, process routing, loop prevention, hot node switching и crash recovery до работы над красивым интерфейсом.

Если этот spike проходит, проект обладает отличием, которого нет у массовых GUI. Если не проходит UDP/DNS/loop isolation, двухъядерная модель не должна маскироваться UI: тогда лучше выпустить supervisor с явными раздельными режимами, чем нестабильный «единый» tunnel.

## Источники

[^1]: XTLS/Xray-core. “[FinalMask implementation](https://github.com/XTLS/Xray-core/tree/main/transport/internet/finalmask).” 2026.
[^2]: MetaCubeX. “[Mihomo routing rules](https://wiki.metacubex.one/en/config/rules/).” 2026.
[^3]: MetaCubeX. “[Mihomo DNS configuration](https://wiki.metacubex.one/en/config/dns/).” 2026.
[^4]: Project X. “[Xray TUN inbound](https://xtls.github.io/en/config/inbounds/tun.html).” 2026.
[^5]: 2dust/v2rayN. “[Architecture & DNS/IPv6 routing behavior in Xray TUN mode](https://github.com/2dust/v2rayN/discussions/9982).” August 2026.
[^6]: Tauri. “[Architecture](https://tauri.app/concept/architecture/).” 2026.
[^7]: 2dust. “[v2rayN](https://github.com/2dust/v2rayN).” 2026.
[^8]: 2dust. “[Description of some UI](https://github.com/2dust/v2rayN/wiki/Description-of-some-ui).” 2026.
[^9]: Clash Verge Rev. “[Project README and features](https://github.com/clash-verge-rev/clash-verge-rev).” 2026.
[^10]: LibNyanpasu. “[Clash Nyanpasu](https://github.com/LibNyanpasu/clash-nyanpasu).” 2026.
[^11]: Mihomo Party Org. “[Clash Party](https://github.com/mihomo-party-org/clash-party).” 2026.
[^12]: chen08209. “[FlClash](https://github.com/chen08209/FlClash).” 2026.
[^13]: Hiddify. “[Hiddify App](https://github.com/hiddify/hiddify-app).” 2026.
[^14]: Throne Project. “[Throne](https://github.com/throneproj/Throne).” 2026.
[^15]: GUI for Cores. “[GUI.for.Clash](https://github.com/GUI-for-Cores/GUI.for.Clash).” 2026.
[^16]: GUI for Cores. “[Plugin Hub](https://github.com/GUI-for-Cores/Plugin-Hub).” 2026.
[^17]: InvisibleManVPN. “[Invisible Man XRay Client](https://github.com/InvisibleManVPN/InvisibleMan-XRayClient).” 2026.
[^18]: LorenEteval. “[Furious](https://github.com/LorenEteval/Furious).” 2026.
[^19]: mhyrzt. “[XRAT](https://github.com/mhyrzt/xrat).” September 2026.
[^20]: MetaCubeX. “[Mihomo TUN](https://wiki.metacubex.one/en/config/inbound/tun/).” 2026.
[^21]: Project X. “[Xray routing](https://xtls.github.io/en/config/routing).” 2026.
[^22]: XTLS/Xray-core. “[TUN incorrectly selects Hyper-V interface](https://github.com/XTLS/Xray-core/issues/6030).” April 2026.
[^23]: Project X. “[Tunnel (formerly dokodemo-door)](https://xtls.github.io/en/config/inbounds/tunnel.html).” 2026.
[^24]: Project X. “[Xray SOCKS inbound](https://xtls.github.io/en/config/inbounds/socks.html).” 2026.
[^25]: MetaCubeX. “[Mihomo SOCKS outbound](https://wiki.metacubex.one/en/config/proxies/socks/).” 2026.
[^26]: Project X. “[Xray API Interface](https://xtls.github.io/en/config/api.html).” 2026.
[^27]: Slint. “[Rust API and platform overview](https://docs.slint.dev/latest/docs/rust/slint/).” 2026; “[Licensing FAQ](https://slint.dev/faqs).” 2026.
[^28]: iced-rs. “[Iced](https://github.com/iced-rs/iced).” 2026.
[^29]: emilk. “[egui](https://github.com/emilk/egui).” 2026.
[^30]: 2dust/v2rayN. “[Mihomo config modification discussion](https://github.com/2dust/v2rayN/discussions/7399).” June 2025.
[^31]: Mozilla. “[MPL 2.0 FAQ](https://www.mozilla.org/en-US/MPL/2.0/FAQ/).” 2026; XTLS/Xray-core, “[README](https://github.com/XTLS/Xray-core),” 2026.
[^32]: MetaCubeX. “[Mihomo README, Meta branch](https://github.com/MetaCubeX/mihomo/blob/Meta/README.md).” 2026.
[^33]: Free Software Foundation. “[GNU GPL FAQ: aggregate and separate programs](https://www.gnu.org/licenses/gpl-faq.html.en#MereAggregation).” 2026.
