# Протокол Daly Smart BMS (WiFi-модуль Hlktech)

Спецификация протокола обмена данными BMS Daly серии `R24TK` (LTO, до 24S),
подключённого к сети через WiFi-модуль Hlktech (`HlktechDevice`). Модуль
периодически опрашивает BMS по Modbus RTU и выкладывает сырые кадры в открытом
HTTP на облачный сервис `databms.com`. Вся телеметрия и вся конфигурация BMS
читаются из этих кадров.

Пример устройства, по которому составлена спецификация:

- Модель платы (MachineCode): `R24TK1A-24S100A` (LTO, 12 ячеек)
- SoftwareVersion `60_250324_01T3`, HardwareVersion `JHB-R24TK-V1.3`
- Прошивка WiFi-модуля `app-1.6.2`, BLE/HLK `3.3.7`

## 1. Архитектура обмена

WiFi-модуль использует несколько независимых каналов:

| Канал | Транспорт | Назначение |
|-------|-----------|------------|
| **Телеметрия BMS** | HTTP POST → `www.databms.com` (порт 80) | Сырые Modbus-кадры BMS в JSON, **открытый текст** |
| Управление/облако | MQTT → Aliyun IoT (порт 1883) | «Thing model» Aliyun Link Kit, TLS 1.2 |
| Обнаружение | CoAP multicast (порт 5683) | Alink `device.info.notify` — анонс устройства в LAN |

Полная телеметрия и конфигурация BMS доступны в HTTP-канале `databms.com`
в открытом виде; MQTT-канал (шифрованный) для их получения не нужен. Мобильное
приложение в режиме «Remote connection» читает те же данные через облако Aliyun.

## 2. HTTP-транспорт (databms.com)

Модуль открывает короткое TCP-соединение (`Connection: close`) и делает POST.
Два эндпоинта.

### 2.1. Телеметрия — `POST /api/v2/http2/SaveThingInfo1`

```
POST /api/v2/http2/SaveThingInfo1 HTTP/1.1
Host: www.databms.com
User-Agent: HlktechDevice
Accept: application/json
Connection: close
Nonce: 5503
Signature: 7782f64027a43b4eebd9bd71889ceb6290a0b90f
TimeStamp: 1782884514388
MesCount: 0
CurrentStatus: 0
Memory: 17048
Content-Length: 1178
Content-Type: application/json

{"DeviceName":"<aliyun-device-name>","Sn":"<serial>","Data":[
  {"Command":"D203008000705665","TimeStamp":"1782884511187","Data":"D203E00190...FC52"},
  {"Command":"D2030000007ED649","TimeStamp":"1782884514214","Data":"D203FC0893...9F50"}
]}
```

Поля тела:

- `DeviceName` — имя устройства в Aliyun.
- `Sn` — серийный номер BMS.
- `Data[]` — массив пар «запрос → ответ» по Modbus:
  - `Command` — hex Modbus-**запроса** к BMS;
  - `Data` — hex Modbus-**ответа** BMS;
  - `TimeStamp` — метка времени (unix, мс).

### 2.2. Регистрация устройства — `POST /api/v2/http2/SaveThing`

`Content-Type: application/x-www-form-urlencoded`, тело — метаданные модуля:

```
DeviceName=DL-<mac>&SoftwareVersion=60_250324_01T3&
HardwareVersion=JHB-R24TK-V1.3&MachineCode=R24TK1A-24S100A&
ManufactureDate=2025-06-29&IotId=<serial>&AliIotId=<aliyun-device-name>&
Mac=1C:78:4B:DC:67:C4&ThingType=1&BleVersion=3.3.7&HlkVersion=3.3.7&BleType=B35
```

### 2.3. Заголовки аутентификации

- `Signature` — 40 hex-символов (SHA-1 / HMAC-SHA1); меняется вместе с `Nonce`
  и `TimeStamp`. Точная схема подписи требует секрета прошивки.
- `Nonce` — случайное число.
- `TimeStamp` — unix-время (мс).
- `Memory` — свободная память модуля (байт), `MesCount` — счётчик сообщений.

## 3. Кадр Modbus RTU

Поля `Command`/`Data` — стандартный **Modbus RTU**, функция `0x03`
(Read Holding Registers). Адрес ведомого (BMS) — **`0xD2`**.

### Запрос (Command)

```
D2 03 | 00 80 | 00 70 | 56 65
└─┬┘   └──┬─┘   └──┬─┘   └─┬─┘
 адрес  старт-  кол-во   CRC16
 +func  регистр регистров (LE)
```

- `D2` — адрес BMS
- `03` — функция (чтение holding-регистров)
- `00 80` — стартовый регистр (big-endian)
- `00 70` — количество регистров (0x70 = 112)
- `56 65` — CRC16, little-endian (значение `0x6556`)

### Ответ (Data)

```
D2 03 | E0 | <224 байта данных> | FC 52
└─┬┘   └┬┘  └───────┬─────────┘  └─┬─┘
адрес  кол-во   N регистров ×      CRC16
+func  байт     2 байта (BE)       (LE)
```

- `E0` = 224 байта = 112 регистров × 2.
- Каждый регистр — 16 бит **big-endian**.
- CRC16 — в конце, little-endian.

### Запросы к двум блокам

| Command | Старт | Кол-во | Блок |
|---------|-------|--------|------|
| `D2 03 00 00 00 7E …` | 0x0000 | 126 (0x7E) | Realtime-данные (§4) |
| `D2 03 00 80 00 70 …` | 0x0080 | 112 (0x70) | Конфигурация (§5) |

### CRC16 (Modbus)

Классический Modbus CRC-16, полином `0xA001` (reflected), init `0xFFFF`,
передаётся младшим байтом вперёд.

```python
def crc16(data: bytes) -> int:
    crc = 0xFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0xA001 if crc & 1 else crc >> 1
    return crc  # передаётся как два байта little-endian
```

## 4. Блок 0x0000 — realtime-данные

Регистры 16-битные, big-endian. Незаполненные слоты (лишние ячейки, датчики)
передаются как `0000`. Поля с пометкой «(?)» идентифицированы не полностью.

| Рег. | Поле | Формула / ед. | Пример |
|------|------|---------------|--------|
| 0x0000–0x001F | Напряжения ячеек (слоты 1…32) | мВ (u16); 0 = слот пуст | `0893`→2195 мВ |
| 0x0020–0x0027 | Температуры внешних датчиков 1…8 | °C = `raw − 40`; 0 = нет | `0035`→13 °C |
| 0x0028 | Напряжение пакета | В = `raw × 0.1` | `010D`→26.9 V |
| 0x0029 | Ток | А = `(raw − 30000) × 0.1`; >0 заряд, <0 разряд | `7534`→+0.4 A |
| 0x002A | SOC | % = `raw × 0.1` | `034A`→84.2 % |
| 0x002B | Макс. напряжение ячейки | мВ | `0AD7`→2775 мВ |
| 0x002C | Мин. напряжение ячейки | мВ | `0891`→2193 мВ |
| 0x002D | Макс. температура | °C = `raw − 40` | `0036`→14 °C |
| 0x002E | Мин. температура | °C = `raw − 40` | `0035`→13 °C |
| 0x002F | Счётчик балансировки (?) | — | `0000`…`0014` |
| 0x0030 | Остаточная ёмкость | Ач = `raw × 0.1` | `0150`→33.6 Ah |
| 0x0031 | Кол-во ячеек | шт | `000C`→12 |
| 0x0032 | Кол-во внешних датчиков темп. | шт | `0002`→2 |
| 0x0033 | Число циклов | шт | `0004`→4 |
| 0x0034 | Балансир активен (= 0x3F) | 0/1 | `0001` |
| 0x0035 | Charge MOS | 0/1 | `0001`→ON |
| 0x0036 | Discharge MOS | 0/1 | `0001`→ON |
| 0x0037 | Среднее напряжение ячейки | мВ | `0898`→2200 мВ |
| 0x0038 | Разброс напряжений (Δ = max − min) | мВ | `0246`→582 мВ |
| 0x0039 | Резерв балансировки (?) | — | `0000`…`002B` |
| 0x003A | Резерв (?) | — | `0000`…`0002` |
| 0x003B | Битовая маска алармов (§4.2) | битовая маска | `0100`, `0200` |
| 0x0040 | Ток балансировки | А = `(raw − 30000) × 0.1` | `7538`→+0.8 A |
| 0x0041 | Кол-во балансируемых ячеек | шт | `000C`→12 |
| 0x0042 | Температура MOS | °C = `raw − 40` | `003B`→19 °C |
| 0x0043–0x0044 | Резерв | `FFFF` | — |
| 0x0057–0x005D | Серийный номер, ASCII | текст | `"224KE220900366"` |

Температур три: T1 (0x0020), T2 (0x0021) — внешние датчики (учитываются в
кол-ве датчиков 0x0032), и температура MOS (0x0042) — отдельно. Формула
`raw − 40` для всех.

### 4.1. Активная балансировка

BMS оснащён активным балансиром. Работой управляют:

- **0x00CF** (конфиг-блок) — разрешение балансировки (enable, 1/0);
- **0x0034** (= 0x003F) — флаг «балансир сейчас работает»;
- **0x0040** — ток балансировки, кодировка та же, что у тока пакета
  (`(raw − 30000) × 0.1` А);
- **0x0041** — число ячеек, участвующих в балансировке.

Ток балансировки (0x0040) течёт независимо от тока пакета: балансир
перекачивает заряд между ячейками, даже когда ток пакета (0x0029) равен нулю.

### 4.2. Маска алармов 0x003B

Битовая маска предупреждений (блок «Status information» в приложении):

| Бит | Значение |
|-----|----------|
| `0x0100` | Разброс напряжений ячеек, уровень 1 (Diff volt level 1) |
| `0x0200` | Разброс напряжений ячеек, уровень 2 / перенапряжение (?) |

`0x0000` — предупреждений нет. Прочие биты (UV, перегрев, перегрузка по току)
в спецификации пока не определены.

## 5. Блок 0x0080 — конфигурация

Регистры 16-битные, big-endian. Защиты хранятся **парами
`(warning, protection)`**: приложение показывает только уровень protection,
BMS держит рядом уровень предупреждения. Единицы: ток `(raw − 30000) × 0.1` А,
температура `raw − 40` °C, напряжение ячейки — мВ, напряжение пакета — `×0.1 V`.

### 5.1. Идентификация и топология

| Рег. | Поле | Ед. | Пример |
|------|------|-----|--------|
| 0x0080 | Rated capacity | Ач ×0.1 | `0190`→40.0 Ah |
| 0x0081 | Cell reference volt | мВ | `08FC`→2.30 V |
| 0x0082 | Collect boards num | шт | `0000`→0 |
| 0x0083 | Battery strings (board 1 cell num) | шт | `000C`→12 |
| 0x0084 | Board 2 cell num | шт | `0000`→0 |
| 0x0085 | Board 3 cell num | шт | `0000`→0 |
| 0x0086 | Board 1 temp num | шт | `0002`→2 |
| 0x0087 | Board 2 temp num | шт | `0000`→0 |
| 0x0088 | Board 3 temp num | шт | `0000`→0 |
| 0x0089 | Тип батареи (2 = LTO?) | код | `0002` |
| 0x008A | Sleep waiting time | с | `FFFF`→65535 |

### 5.2. Защиты (warning / protection)

| Рег. | Поле | Пример |
|------|------|--------|
| 0x008B | Cell high — warning | `0AF0`→2.80 V |
| 0x008C | Cell high — protection | `0B22`→2.85 V |
| 0x008D | Cell low — warning | `0780`→1.92 V |
| 0x008E | Cell low — protection | `074E`→1.87 V |
| 0x008F | Pack high — warning | `011F`→28.7 V |
| 0x0090 | Pack high — protection | `012B`→29.9 V |
| 0x0091 | Pack low — warning | `00DE`→22.2 V |
| 0x0092 | Pack low — protection | `00D2`→21.0 V |
| 0x0093 | Discharge overcurrent — warning | `73F0`→−32.0 A |
| 0x0094 | Discharge overcurrent — protection | `73A0`→−40.0 A |
| 0x0095 | Charge overcurrent — warning | `7670`→+32.0 A |
| 0x0096 | Charge overcurrent — protection | `76C0`→+40.0 A |
| 0x0097 | Charge high temp — warning | `0055`→45 °C |
| 0x0098 | Charge high temp — protection | `005F`→55 °C |
| 0x0099 | Charge low temp — warning | `0023`→−5 °C |
| 0x009A | Charge low temp — protection | `0019`→−15 °C |
| 0x009B | Discharge high temp — warning | `0055`→45 °C |
| 0x009C | Discharge high temp — protection | `005A`→50 °C |
| 0x009D | Diff temp protect (дельта, без offset) | `000A`→10 °C |
| 0x009E | Discharge low temp — protection | `0000`→−40 °C |
| 0x00A0 | Differential pressure alarm | `0320`→0.80 V |
| 0x00A8 | Fan On temperature | `0057`→47 °C |

Разряд кодируется значением `< 30000`, поэтому защиты по току разряда
(0x0093/0x0094) отрицательны, по току заряда (0x0095/0x0096) — положительны.

### 5.3. Балансировка, версии, служебные

| Рег. | Поле | Пример |
|------|------|--------|
| 0x00A2 | Balance current | `000A`→1.0 A |
| 0x00A3 | Balanced open start volt | `0708`→1.80 V |
| 0x00A4 | Balanced open diff volt | `0014`→0.02 V |
| 0x00A7 | SOC (дублирует realtime 0x002A) | `035B`→85.9 % |
| 0x00CF | Balance enable (тумблер) | `0001`=вкл, `0000`=выкл |
| 0x00A9–0x00AF | Строка версии, ASCII | `"3T10_423052_06"` |
| 0x00B1–0x00B7 | Версия HW, ASCII | `"3.1V-KT42R-BHJ"` |
| 0x00B9–0x00C0 | MachineCode, ASCII | `"R24TK1A-24S100A"` |
| 0x00C9–0x00CB | Код/пароль, ASCII | `"123456"` |
| 0x00CC–0x00CD | Дата производства (Y-M-D, hex) | `1906 1D` → 2025-06-29 |
| 0x00D4–0x00D6 | RTC (Y-M-D h:m:s, hex, побайтно) | — |

Дата и RTC кодируются побайтно в hex: год (от 2000), месяц, день, часы,
минуты, секунды (например `1A 07 01 15 1D 2C` = 2026-07-01 21:29:44).

Не идентифицированы: регистры `0x009F`, `0x00A1`, `0x00A5`, `0x00A6`,
а также настройки Equalized cut-off voltage, Heating On/Off temperature,
тип батареи (предположительно `0x0089`).

## 6. Пример декодирования

Взять из JSON поле `Data`, снять заголовок Modbus (`D2 03 <bytecount>`) и хвост
CRC (2 байта), разбить остаток на 16-битные big-endian слова, применить формулы
из §4–§5.

```python
def parse_modbus_response(hexstr: str) -> list[int]:
    raw = bytes.fromhex(hexstr)
    assert raw[0] == 0xD2 and raw[1] == 0x03   # адрес, функция
    nbytes = raw[2]
    payload = raw[3:3 + nbytes]                # без CRC
    return [int.from_bytes(payload[i:i+2], "big")
            for i in range(0, len(payload), 2)]

regs = parse_modbus_response("D203FC0893...9F50")  # блок 0x0000
pack_v    = regs[0x28] * 0.1                   # В
current   = (regs[0x29] - 30000) * 0.1         # А (>0 заряд, <0 разряд)
soc       = regs[0x2A] * 0.1                   # %
cap_ah    = regs[0x30] * 0.1                   # Ач
cells     = [regs[i] for i in range(0x20) if regs[i]]           # мВ
temps     = [regs[i] - 40 for i in range(0x20, 0x28) if regs[i]]  # °C, внешние
mos_temp  = regs[0x42] - 40                    # °C, MOS
cycles    = regs[0x33]
chg_mos   = bool(regs[0x35])
dis_mos   = bool(regs[0x36])
balancing = bool(regs[0x34])
alarms    = regs[0x3B]                          # 0x0100 = Diff volt level 1
```

## 7. Прочие каналы

- **MQTT ↔ Aliyun IoT** (порт 1883) — «thing model» Aliyun Link Kit поверх
  TLS 1.2. Дублирует телеметрию из HTTP-канала; для чтения данных BMS не нужен.
- **CoAP-анонс** (`/sys/device/info/notify`, порт 5683) — не содержит значений
  BMS, только идентификацию устройства: `productKey`, `deviceName`, `mac`, `ip`,
  `token`, `fwVersion`.

## 8. Незакрытые пункты

- Регистры-счётчики realtime-блока: `0x002F`, `0x0039`, `0x003A`.
- Прочие биты маски алармов `0x003B` (UV, перегрев, перегрузка по току).
- Неидентифицированные настройки блока 0x0080 (см. конец §5.3).
- Схема заголовка `Signature` HTTP-запросов.
