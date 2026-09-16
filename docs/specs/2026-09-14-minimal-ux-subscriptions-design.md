# Минимальный UX и контракт подписок

## Решение

Клиент выглядит как одно простое приложение, а не как панель управления Xray и Mihomo. Два ядра, их порты, мосты и конфигурации являются внутренней реализацией.

Из трёх вариантов выбран **одноэкранный shell с выдвижной диагностикой**:

1. Полный dashboard удобен разработчику, но перегружает обычное подключение — отклонён.
2. Одно окно с одной главной кнопкой и скрываемыми подробностями сохраняет простоту и даёт путь к диагностике — выбран.
3. Tray-only минимален, но плохо объясняет первый запуск и ошибки — отклонён как основной интерфейс.

## Главный экран

В обычном состоянии на экране находятся только:

- статус: `Не подключено`, `Подключение…`, `Подключено`, `Нужна помощь`;
- большая контекстная кнопка `Подключиться` / `Отключиться` / `Повторить`;
- текущий профиль;
- текущая группа или режим `Авто`;
- одна строка текущего маршрута, например `Telegram → Proxy → NL-1 · FinalMask`;
- кнопка `Подробнее`.

Никаких отдельных переключателей Xray/Mihomo, портов, TUN и DNS на главном экране нет.

Состояние кнопки является конечным автоматом:

```text
EMPTY → IMPORTING → READY → CONNECTING → CONNECTED
                    ↑           ↓            ↓
                    └──────── ERROR ←────────┘
```

Кнопка всегда предлагает ровно одно следующее действие.

## Первый запуск и URL schemes

Первый запуск состоит из трёх шагов:

1. Добавить ссылку подписки или принять ссылку из `multicore://install-sub?url=...`.
2. Автоматически загрузить, проверить и сопоставить конфигурации.
3. Показать имя профиля, число доступных узлов и кнопку `Подключиться`.

Development identity использует scheme `multicore://`. Нативный bundle запрашивается с `multicore-json-massive`; раздельный dual-fetch использует `multicore-mihomo` для Mihomo и `multicore-xray` для Xray.

Поддерживаемые действия scheme в v1:

- `multicore://install-sub?url=<percent-encoded-url>` — добавить или обновить подписку;

Ссылка передаётся уже запущенному singleton daemon через локальный IPC. Импорт из браузера требует подтверждения с показом домена. URL и subscription tokens никогда не попадают в UI-логи. Импорт отдельных нод и захват `vless://`, `vmess://` или `trojan://` не поддерживаются.

## Получение подписки

### Быстрый нативный путь

Сначала выполняется один запрос:

```http
User-Agent: multicore-json-massive
Accept: application/vnd.multicore.bundle+json, application/json
```

Заголовок User-Agent намеренно стабилен и не содержит версии, чтобы сервер мог сравнивать его точно. Версия и платформа передаются отдельно:

```http
X-Client-Version: 0.1.0
X-Client-Platform: windows-x86_64
```

Ответ — JSON tuple из двух элементов: сначала строка с Mihomo YAML, затем готовый Xray JSON. Xray-часть считается opaque payload: клиент проверяет только UTF-8 и синтаксис JSON, затем сохраняет её без изменений.

```json
[
  "proxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [NL-1]\nrules:\n  - MATCH,Proxy",
  {
    "outbounds": [
      {
        "tag": "NL-1",
        "protocol": "freedom",
        "settings": {}
      }
    ]
  }
]
```

Типы и порядок фиксированы: `[YAML string, JSON value]`. Сервер заранее создаёт в Xray по одному именованному SOCKS inbound/порту на каждый Shadowsocks+FinalMask outbound и помещает соответствующие SOCKS proxies в Mihomo YAML. Клиент не строит и не проверяет это соответствие.

### Раздельный fallback

Если ответ `multicore-json-massive` отсутствует или не проходит строгую проверку tuple, одновременно выполняются два GET одного URL:

- `User-Agent: multicore-mihomo` — body обязан быть Mihomo YAML;
- `User-Agent: multicore-xray` — body обязан быть синтаксически валидным Xray JSON.

UA является частью cache key, а `ETag` и `Last-Modified` хранятся отдельно для каждого `(URL, User-Agent)`.

Любой другой формат отклоняется. Клиент не разбирает base64-списки, share links, Clash JSON, sing-box JSON и не вызывает внешние subscription converters.

Обе части сначала попадают во временный snapshot. Активный профиль заменяется только когда обе части:

- успешно скачаны;
- прошли лимиты размера и кодировки;
- Mihomo body распарсился как YAML mapping;
- Xray body распарсился как JSON, но его поля не интерпретировались;
- готовы к атомарной публикации как неизменённые runtime-файлы.

Только Mihomo YAML разбирается в domain model для групп, proxies, выбранных узлов и UI. Xray JSON не нормализуется, не сериализуется повторно и не пропускается через клиентскую схему — FinalMask и любые серверные поля сохраняются как есть.

Последний рабочий snapshot продолжает использоваться при сетевой ошибке. Новый повреждённый ответ никогда не затирает рабочую конфигурацию.

## Переопределения

Raw YAML/JSON не является пользовательским интерфейсом. Модель слоёв:

```text
subscription snapshot
  → typed Mihomo user overrides
  → ephemeral runtime values
```

В обычном UI доступны только выбор профиля, группы и ноды. Экран `Расширенные настройки` позже может добавить типизированные исключения по приложению, IPv6 и DNS. Произвольный редактор конфигов не входит в v1.

## Логи и диагностика

Xray, Mihomo и Rust daemon пишут в единый поток событий:

```text
timestamp · severity · component · phase · profile_id · node_id
correlation_id · safe_message · structured_details
```

На главном экране показываются только три последних пользовательских события:

```text
✓ Профиль обновлён
✓ Узел NL-1 готов
✓ Интернет направлен через VPN
```

`Подробнее` открывает диагностический экран с фильтрами `Все / Подписка / Маршруты / Mihomo / Xray`. Raw-строки доступны через раскрытие события, но секреты редактируются до попадания на диск.

Хранение v1:

- кольцевой буфер последних 10 000 событий в памяти;
- пять файлов по 2 MiB;
- support bundle только по явному действию пользователя;
- URL query, Authorization/Cookie, UUID, passwords и subscription tokens заменяются на `[redacted]`.

## Ошибки

Пользователь видит действие, а не stack trace:

- `Подписка не поддерживает Xray-конфигурацию`;
- `Не удалось сопоставить 2 узла`;
- `TUN не запущен — Повторить`;
- `Соединение восстановлено через последний рабочий профиль`.

Каждая ошибка содержит внутренний correlation ID для поиска подробностей.

## Failure-mode check

1. **`multicore-json-massive` недоступен или не поддерживается сервером.** Клиент атомарно запрашивает пару `multicore-mihomo` и `multicore-xray`. Успешный HTTP-ответ massive является терминальным: невалидный payload возвращает ошибку без дополнительных запросов.
2. **Две fallback-выдачи относятся к разным ревизиям.** Критично: клиент может гарантировать только совместную локальную публикацию. Строгую серверную атомарность гарантирует `multicore-json-massive`.
3. **Логи раскрывают секреты.** Критично: structured redaction выполняется до persistence, export дополнительно сканируется.
4. **Главный экран превращается в dashboard.** Критично для продукта: новые технические показатели допускаются только в `Подробнее`.

## Проверка реализации

- contract tests для `[Mihomo YAML string, Xray JSON value]`, неверного порядка, невалидного JSON и size limits;
- HTTP tests для fallback, redirect policy, timeout, ETag per UA и partial failure;
- fixture tests, доказывающие byte-for-byte сохранение Xray JSON и извлечение групп только из Mihomo YAML;
- rejection tests для base64, share links, Clash JSON и sing-box JSON;
- snapshot tests конечного автомата главной кнопки;
- redaction tests с UUID, credentials, cookies и URL tokens;
- integration test: предыдущий snapshot остаётся активным при повреждённом обновлении.

## Не входит в v1

- графический редактор YAML/JSON;
- графики трафика на главном экране;
- плагины и скриптовые overrides;
- импорт отдельных proxy URL и base64-подписок;
- автоматическая конвертация сторонних форматов;
- ручное управление процессами Xray и Mihomo.

## Runtime DNS bootstrap amendment (2026-09-16)

The import boundary remains unchanged: Mihomo YAML is the only payload parsed for groups/UI, and the fetched Xray JSON is persisted byte-for-byte. Immediately before each Connect, a separate ephemeral Xray copy is minimally inspected for Shadowsocks server addresses. Domain-valued endpoints are resolved on the client while Mihomo TUN is still down; their IPs are merged into `dns.hosts`, matching outbounds receive `sockopt.domainStrategy: ForceIP`, and all outbound sockets receive the current native Windows default `sockopt.interface`. FinalMask and every unrelated server field survive unchanged. Failure to resolve a domain or interface aborts Connect before either core starts and never replaces the last-good snapshot.
