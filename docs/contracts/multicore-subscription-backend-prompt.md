# Промпт для backend подписок MultiCore

Скопируй текст ниже целиком в задачу backend-разработчику.

---

Реализуй один endpoint подписки MultiCore. Один и тот же URL должен возвращать разные представления одной и той же ревизии конфигурации в зависимости от точного значения HTTP-заголовка `User-Agent`.

Поддерживаются ровно три User-Agent:

- `multicore-mihomo`
- `multicore-xray`
- `multicore-json-massive`

Любой другой User-Agent возвращает HTTP `406 Not Acceptable`. Не выполняй эвристическое распознавание клиентов и не добавляй другие форматы.

## 1. Ответ для `multicore-mihomo`

Верни полный сырой Mihomo YAML в UTF-8. Это не список ссылок, не base64 и не JSON.

YAML должен быть полностью готов к запуску Mihomo и содержать:

- TUN;
- DNS;
- sniffing;
- правила маршрутизации, включая правила по процессам;
- `rule-providers`, если они нужны профилю;
- `proxy-groups` и выбранные селекторы;
- локальные SOCKS5 proxies, через которые Mihomo передаёт трафик в Xray.

Для каждого серверного Shadowsocks+FinalMask outbound создай в Mihomo отдельный SOCKS5 proxy:

```yaml
proxies:
  - name: "NL-1"
    type: socks5
    server: 127.0.0.1
    port: 31001
    udp: true
```

`name` и `port` должны совпадать с соответствующим локальным SOCKS inbound в Xray-представлении. Имена должны быть уникальны и пригодны для показа пользователю. Порты должны быть уникальны внутри профиля и находиться в разрешённом backend диапазоне.

Заголовок ответа:

```http
Content-Type: application/yaml; charset=utf-8
```

## 2. Ответ для `multicore-xray`

Верни один готовый к запуску объект Xray JSON в UTF-8. Не возвращай массив конфигов, строку с JSON, base64 или список ссылок.

Конфигурация должна содержать:

- Shadowsocks+FinalMask outbounds;
- по одному локальному SOCKS inbound на каждый такой outbound;
- routing rule, которая однозначно направляет каждый SOCKS inbound в его outbound;
- при необходимости служебные `direct`/`block` outbounds.

Каждый локальный inbound слушает только loopback:

```json
{
  "tag": "NL-1",
  "listen": "127.0.0.1",
  "port": 31001,
  "protocol": "socks",
  "settings": {
    "auth": "noauth",
    "udp": true,
    "ip": "127.0.0.1"
  }
}
```

Связь должна быть явной через `inboundTag` и `outboundTag`:

```json
{
  "type": "field",
  "inboundTag": ["NL-1"],
  "outboundTag": "NL-1"
}
```

Backend является единственным источником истины для соответствия `display name → SOCKS inbound tag → local port → Xray outbound tag`. Клиент это соответствие не строит и семантически не проверяет.

Не добавляй Xray TUN, Xray DNS-policy или маршрутизацию по процессам: ими полностью владеет Mihomo.

Заголовок ответа:

```http
Content-Type: application/json; charset=utf-8
```

## 3. Ответ для `multicore-json-massive`

Верни JSON-массив ровно из двух элементов и строго в таком порядке:

```json
[
  "<полный Mihomo YAML как JSON-строка>",
  {
    "inbounds": [],
    "outbounds": [],
    "routing": { "rules": [] }
  }
]
```

Первый элемент — строка. После JSON-декодирования она должна быть побайтно равна body ответа `multicore-mihomo` для той же ревизии.

Второй элемент — JSON object. Он должен быть семантически равен body ответа `multicore-xray` для той же ревизии. Не заключай его в строку.

Заголовок ответа:

```http
Content-Type: application/vnd.multicore.bundle+json; charset=utf-8
```

## Атомарность и HTTP-заголовки

Все три представления должны генерироваться из одного immutable snapshot. На каждом успешном ответе возвращай:

```http
X-Multicore-Revision: <неизменяемый идентификатор ревизии>
ETag: "<etag конкретного представления>"
Cache-Control: private, no-cache
```

Для одной ревизии значение `X-Multicore-Revision` одинаково во всех трёх ответах. `ETag` может отличаться, потому что тела различаются. Если backend отдаёт сведения о трафике и сроке подписки, используй стандартный `Subscription-Userinfo`.

Сначала полностью собери и провалидируй обе части snapshot, затем атомарно опубликуй его. Никогда не выдавай Mihomo и Xray из разных ревизий.

Максимальный размер каждого отдельного body — 32 MiB. Установи разумные timeout и rate limit. Не записывай в логи URL подписки, query-параметры, UUID, ключи, пароли, содержимое конфигов и заголовки авторизации.

## Обязательные проверки перед публикацией

1. Mihomo body является UTF-8 YAML mapping и успешно разбирается Mihomo.
2. Xray body является одним JSON object и проходит `xray run -test -config <file>` на целевой версии ядра.
3. Для каждого bridge-узла имя и порт SOCKS proxy в Mihomo совпадают с SOCKS inbound в Xray.
4. Все inbound tags, outbound tags, отображаемые имена и локальные порты уникальны там, где требуется.
5. Каждый bridge inbound слушает только `127.0.0.1`.
6. Каждый bridge inbound имеет ровно один маршрут в соответствующий Shadowsocks+FinalMask outbound.
7. Combined-ответ содержит ровно два элемента правильных типов и соответствует отдельным ответам той же ревизии.
8. Неизвестный User-Agent получает 406.
9. Секреты не попадают в application/access/error logs.

Добавь автоматические contract tests на все пункты выше, включая несовпадающие имена/порты, дубли портов, Xray array вместо object, переставленные элементы combined tuple, частично собранный snapshot и разные revision headers.

Клиентская модель фиксирована: клиент парсит только Mihomo YAML для групп, прокси и UI; Xray JSON проверяет только на UTF-8 и синтаксис JSON, сохраняет без преобразования и запускает как есть. Порядок запуска: сначала Xray, затем Mihomo. Порядок остановки: сначала Mihomo, затем Xray.

---
