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
`frames_decoded`, `frames_dropped`, `last_frame_timestamp`), **device_info**,
**счётчик балансировки** (`balance_amp_hours`) и **автокалибровка датчика тока**
(`src/calibration.rs`, см. ниже).
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
- гейт правдоподобия (`RealtimeData::implausibility`, вызывается в
  `server.rs` **до** любого обновления метрик): CRC-валидный кадр может нести
  один мусорный регистр (в проде — 436.6 А при нормальных ячейках), такой кадр
  отбрасывается целиком в `frames_dropped_total{reason="implausible_*"}`;
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
make grafana REMOTE=<host>        # установка Grafana-дашбордов на хост
make rules REMOTE=<host>          # синхронизация Prometheus alert rules
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
`max_body_bytes`, `request_timeout_secs`, `coulomb_max_gap_secs`, `max_devices`,
`coulomb_state_path`, `coulomb_state_min_interval_secs`,
`max_plausible_current_amperes`, `min/max_plausible_pack_volts`,
`max_frame_amp_hours`, `max_frame_watt_hours`, `calibration_enabled`,
`calibration_tau_hours`, `calibration_min_span_hours`,
`calibration_max_offset_amperes`, `calibration_max_anchor_error_amperes`,
`calibration_peer_max_disagreement_amperes` (см. `config.example.yaml`).

**Инвариант счётчиков энергии (не ломать):** значение в state-файле всегда ≥
уже отданного в `/metrics`. Prometheus трактует любое уменьшение counter'а как
reset и добавляет весь накопленный total в `increase()` — на проде это дало
фантомный столбик +20 кВт·ч. Держат инвариант три вещи: feature
`float_roundtrip` у `serde_json` (без неё парсер теряет 1 ULP примерно на 12%
значений — это и была первопричина), запись state-файла до инкремента counter'а
(`Metrics::flush_coulomb_state`) и `bump_ulp`. Запись — durable (fsync tmp +
fsync каталога) и выполняется **вне** мьютекса: рантайм однопоточный, и fsync
под локом вешает в том числе `/metrics`. Отметка времени записи ставится на
каждую **попытку**, а не только на успех, иначе сломанный диск заставляет
фсинкать на каждом кадре (в одном POST их может быть под сотню).

## Автокалибровка датчика тока (`src/calibration.rs`)

Ноль датчика тока у каждого BMS смещён по-своему. На проде три параллельных
пакета стояли на −70 / +150 / −60 мА; интеграл +150 мА — это 3.6 Ач/сутки
фантомного заряда, из-за которого energy-in обгонял energy-out на 25% при
неподвижном SOC. Балансировка (см. `balance_amp_hours`) — вторая статья, 9–14%
оборота.

Оценщик решает уравнение баланса заряда
`∫I dt = ΔQ + ∫I_bal dt + offset·T + ε` (`Q` — remaining capacity от BMS),
подбирая наклон невязки экспоненциально взвешенным МНК (`EwFit`, 6 аккумуляторов,
без кольцевого буфера).

**Не ломать при правках:**

- **`Q` ограничен `0..=Cap`** — именно это делает оценку некруговой, несмотря на
  то что `Q` считает тот же смещённый датчик. Отсюда граница ошибки наклона
  `Cap/window`, она экспортируется как `calibration_anchor_error_amperes` и
  служит **гейтом**: коррекция не применяется, пока оценка не доказуемо точна.
  Не заменять этот гейт на таймаут и не гейтить на `stderr` наклона — `y` это
  бегущий интеграл, остатки автокоррелированы, и `stderr` занижен на порядки.
- **Ось `x` — проинтегрированные часы, не настенное время.** Пропуск связи,
  срезанный `coulomb_max_gap_secs`, не должен попадать в `T`, иначе наклон
  размывается к нулю после каждой аварии. По той же причине `max_gap_secs`
  калибратора берётся из `coulomb_max_gap_secs`.
- **Оценщик кормится СЫРЫМ током.** Подать ему скорректированный — замкнуть
  обратную связь, которая утянет offset в ноль.
- **Сырые счётчики неприкосновенны.** Коррекция идёт в отдельные
  `daly_bms_calibrated_*`; на них распространяется тот же инвариант
  «запись state-файла до инкремента counter'а», что и на исходные (поля в
  `CoulombEntry`, всё через `Pending`).
- **Межпакетная регрессия — это валидация, а не второй оценщик.** Она меряет
  ТОЛЬКО ошибку датчика, тогда как баланс заряда вбирает ещё и реальные потери,
  которые балансировщик недоотчитал. Расхождение ~50 мА на проде — норма;
  порог `peer_max_disagreement` ловит грубую поломку и **замораживает** offset.
- Топология параллельных пакетов **не задаётся в конфиге** — пары находятся по
  данным (slope≈1, высокий R²).

`examples/replay_history.rs` прогоняет дамп из Prometheus через оценщик — после
любой правки калибратора проверять на реальной телеметрии, а не только на
синтетике из юнит-тестов.

Пакетирование: `packaging/daly-bms-exporter.service` — systemd-unit (DynamicUser +
`CAP_NET_BIND_SERVICE` + `StateDirectory=daly-bms-exporter` для persistence
кулоновских/энергетических счётчиков); ставится через `make deb` (cargo-deb).

## Ключевые правила декодирования (легко ошибиться)

Формулы кодирования регистров (ток `(raw−30000)×0.1`, температура `raw−40`,
напряжения, SOC/ёмкость `×0.1`, CRC16 `0xA001`, пустые слоты `0000` и т.д.)
вынесены в **`.claude/rules/decoding.md`** (scoped на `src/decode.rs`,
`src/modbus.rs`). Единственный источник истины по протоколу —
`doc/daly-bms-protocol.md`; читать перед любой работой с парсером.

## Стиль

Комментарии, идентификаторы и commit-message — на английском. Русский допустим
только в пользовательской документации (`doc/**`, README).
