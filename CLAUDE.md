# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Что это

Prometheus-exporter телеметрии BMS **Daly R24TK** (LTO, до 24S) через WiFi-модуль
Hlktech. Устройство периодически шлёт сырые Modbus-кадры HTTP POST'ом в открытом
виде на облако `www.databms.com`. Пользователь перенаправляет этот трафик на
локальный порт, который слушает экспортёр; тот парсит кадры и отдаёт метрики в
`/metrics` для Prometheus.

Статус: рабочий, покрытый тестами экспортёр. Реализованы модули `config`,
`decode`, `error`, `metrics`, `modbus`, `payload`, `server` (библиотека + бинарь),
зависимости на месте (axum, tokio, prometheus, serde/serde_norway и др.),
`edition = "2024"`; проект собирается, тестируется и развёрнут.

Реализованы также: **кулоновский счётчик** (charge/discharge amp-hours, интеграл
тока по времени) и **энергетический счётчик** (charge/discharge watt-hours,
интеграл мгновенной мощности `V*I`), оба с дисковой персистентностью
(`coulomb_state_path`), **self-observability метрики** (`http_requests`,
`frames_decoded`, `frames_dropped`, `last_frame_timestamp`), **device_info**.
Есть **Grafana-дашборд** (`grafana/`), **деплой** (`scripts/deploy.sh`,
`make deploy`/`make grafana` на aarch64-хост), **CI/Release**
(`.github/workflows/`, `deny.toml`).

**Инвариант безопасности (не ломать при правках):** вход — untrusted, без auth.
Держать:

- ограничение кардинальности метрик (`Config::accept_serial` +
  `is_plausible_serial` + `Metrics::admit` / `max_devices`);
- санитизацию логов — untrusted-строки логировать через `?`-Debug, не
  `%`-Display;
- bounds-checked декодирование (`regs.get()`, клампы `MAX_CELLS` / `MAX_TEMPS`);
- `#![forbid(unsafe_code)]`.

## Команды

```bash
cargo build                       # сборка
cargo test                        # все тесты
cargo test <name>                 # один тест по имени
cargo test -- --nocapture         # с выводом println!
cargo clippy --all-targets        # линт
cargo fmt                         # форматирование
cargo run                         # запуск бинаря
cargo deny check                  # аудит зависимостей/лицензий (deny.toml)
make deb                          # сборка .deb-пакета
make deploy REMOTE=<host>         # деплой на aarch64-хост (scripts/deploy.sh)
make grafana REMOTE=<host>        # установка Grafana-дашборда на хост
```

## Архитектура и поток данных

Полная спецификация протокола — `doc/daly-bms-protocol.md` (единственный источник
истины по декодированию; читать перед любой работой с парсером).

Поток данных, который реализует экспортёр:

1. **HTTP-приёмник.** Слушает локальный порт. Устройство (`User-Agent:
   HlktechDevice`) шлёт `POST /api/v2/http2/SaveThingInfo1` с JSON-телом
   `{"DeviceName","Sn","Data":[{"Command","Data","TimeStamp"}...]}`. Также есть
   регистрационный `POST /api/v2/http2/SaveThing` (`x-www-form-urlencoded`,
   метаданные модуля) — телеметрии не содержит. Заголовок `Signature` проверять
   не нужно (схема подписи неизвестна), достаточно принимать POST.
2. **Modbus-декодер.** `Command`/`Data` — hex Modbus RTU, функция `0x03`, адрес
   BMS `0xD2`. Ответ: `D2 03 <bytecount> <N регистров по 2 байта BE> <CRC16 LE>`.
   Снять заголовок (3 байта) и хвост CRC (2 байта), разбить на 16-битные
   big-endian слова.
3. **Маппинг регистров → метрики.** По стартовому регистру запроса различать два
   блока и применять формулы из doc:
   - блок `0x0000` (запрос `D2 03 00 00 00 7E`) — realtime (§4);
   - блок `0x0080` (запрос `D2 03 00 80 00 70`) — конфигурация (§5).
4. **Prometheus-экспозиция.** **Один axum-сервер** обслуживает и приём POST'ов
   от устройства, и `GET {metrics_path}` (по умолчанию `/metrics`), отдающий
   последний распарсенный снимок, и `GET /healthz` (health-check).

Конфиг — YAML через **`serde_norway`** (поддерживаемый форк архивного
`serde_yaml`); поля: `listen`, `metrics_path`, `log_level`, `allowed_serials`,
`max_body_bytes`, `request_timeout_secs`, `coulomb_max_gap_secs`, `max_devices`
(см. `config.example.yaml`).

## Ключевые правила декодирования (легко ошибиться)

Формулы кодирования регистров (16 бит, big-endian):

- **Ток:** `(raw − 30000) × 0.1` А. `>0` заряд, `<0` разряд. Та же кодировка у
  тока пакета (0x0029), тока балансировки (0x0040) и всех токовых защит.
- **Температура:** `raw − 40` °C (внешние датчики, MOS, температурные защиты).
  Исключение — дельта-температуры (`0x009D` diff temp) без offset'а.
- **Напряжение ячейки:** мВ напрямую. **Напряжение пакета:** `raw × 0.1` В.
- **SOC / ёмкость:** `raw × 0.1` (% и Ач).
- **CRC16:** Modbus, полином `0xA001` (reflected), init `0xFFFF`, передаётся
  little-endian. Валидировать кадры перед парсингом.
- Пустые слоты (лишние ячейки/датчики) приходят как `0000` — отфильтровывать.
- Защиты в блоке 0x0080 хранятся парами `(warning, protection)` в соседних
  регистрах.
- Часть регистров/битов не идентифицирована (см. §8 doc) — не выдумывать
  семантику, помечать как reserved/unknown.

## Стиль

Комментарии, идентификаторы и commit-message — на английском. Русский допустим
только в пользовательской документации (`doc/**`, README).
