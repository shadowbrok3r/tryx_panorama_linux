# HomeUI Protocol Reference

TRYX Panorama AIO cooler display — Linux/Rust client implementation spec. Synthesized from decompilation of `com.baiyi.homeui.tkcfanhomeui` (HomeUI) and `com.baiyi.service.serialservice` (SerialService). Every claim is backed by a file:line citation from the dimension reports. Where hardware verification is still required, it is marked `[NEEDS-HW]`.

## Transport model (context — already known, restated for completeness)

- Frame: `5A | len | payload | crc | 5A`. Payload is HTTP-like: request `POST <cmdType> 1\r\n<headers>\r\n\r\n<json>`; also `STATE <cmdType> 1` (verb `STATE` is distinct from `POST`). Reply line is `1 200` (version-first).
- `SerialData.STATE_*` constants (`serialservice/.../data/entity/SerialData.java:29-32`): `DELETE="DELETE"`, `GET="GET"`, `POST="POST"`, `STATE="STATE"`.
- SerialService parses every frame, dispatches it locally AND forwards it to HomeUI via AIDL `onSerialDataChanged` (`SerialService.java`). Two processes act on some cmdTypes in parallel (notably `config` and `turboPump`: SerialService does the pump sysfs writes; HomeUI does render/brightness/fanLCD).
- Ack convention: device reply carries `AckNumber = received SeqNumber + 1` (`MRM:108,252`; forwarded verbatim through `SDM:66-77` → AIDL `sendData(SerialData,int)`). SerialService assigns its own SeqNumber and stamps wire headers; HomeUI sets only `version`/`cmdType`/`dataContent` on replies, so all other `DataHeader` fields default to `-1`/`"-1"` (`DataHeader.java:21-47`).
- **Reply source split:** For every POST cmdType EXCEPT `all`, SerialService sends a **generic bodyless `200`** (`SMRH:357-360`: `if (cmdType.equals("all")) return; sendAck(200, seqNumber+1)`). HomeUI itself replies ONLY to `all` (with a JSON status body). `STATE`/`GET`/`DELETE` non-`all` frames also get the generic bare 200 from SerialService.
- **Watchdog:** HomeUI arms a 60 s inactivity timer, re-armed on every received frame (`MRM:410-411,447-448`, `sendEmptyMessageDelayed(102, 60000L)`). On expiry → local `onDisConnect()` (screen-off UI), no transmission. The next frame of any kind triggers `onReConnect()` (`MRM:435-446`). **Client must send `all` at least every 60 s** or the screen drops to its disconnected state.
- **Clock sync side effect of `all`:** if `|now − timestamp| > 300000 ms`, device calls `Util.setAndroidSystemTime` (`MainActivity.full.java:125-129`; `Util.java:55-58` uses `AlarmManager.setTime(ms)`).
- **Fan side effect of `all`:** every `all` push runs `setFanSpeed(cpu.getTemperature())` (see Fan section).
- **No unsolicited device→PC messages exist** (only 2 `sendMsg` call sites in HomeUI, both `all` replies). To observe applied fan duty / pump rpm you must poll `STATE all`.

Sysfs paths used (write with plain `BufferedWriter(FileWriter).write(str)`, `FileUtil.writeData`; read via `readLine()` returning `""` on error):
- Fan duty (HomeUI, write + status read): `/sys/bus/platform/drivers/lcd_fan/speed` (`MRM:39`, `MA:116`)
- Pump rpm (read): `/sys/bus/i2c/drivers/aio_cooler/rpm` (`MRM:40`; `SMRH:53-55` `turboGetPath`)
- Pump control source (SerialService): `/sys/bus/i2c/drivers/aio_cooler/control_source` (`SMRH:53-55`)
- Pump pwm (SerialService): `/sys/bus/i2c/drivers/aio_cooler/pwm` (`SMRH:53-55`)

---

## Command catalog

16 HomeUI cmdTypes (switch index in parentheses). Direction is always PC→device (POST/STATE) unless noted. "Reply" is what the device sends back.

| cmdType (idx) | Verb | Request JSON | Reply | Effect |
|---|---|---|---|---|
| `all` (0) | POST | full `PcInfo` (see sysinfo schema) | HomeUI: `200` body `{"status":{"fanLCD":"<speed file>"}}` (`MRM:238-252`) | Render telemetry; sync clock; run fan curve |
| `all` (0) | STATE | full `PcInfo` | HomeUI: `200` full status body (see Status Reply) | Same render + returns fanLCD/turboPump/warning/storage |
| `power` (1) | POST | `{"event":"suspend"\|"shutdown"\|"lock-screen"\|"resume"\|"unlock-screen"}` | bare 200 | Screen off (first 3) / on (last 2). No real Android power action |
| `waterBlockScreen` (2) | POST | `{"enable":<bool>}` | bare 200 | Pure display on/off. `false`→brightness 0 (never standby video) |
| `displayInSleep` (3) | POST | `{"enable":<bool>}` | bare 200 | Sets flag: true→show standby video on sleep/disconnect; false→black |
| `waterBlockScreenId` (4) | POST | `ScreenConfig` object | bare 200 | Full layout rebuild (media/regions/filters/badges/sysinfo) |
| `preset` (5) | POST | `ScreenConfig` (only `settings`+`sysinfoDisplay` used) | bare 200 | Overlay-only update; media/playMode/ratio/screenMode untouched |
| `disconn` (6) | POST | (none parsed) | bare 200 | Graceful screen-off; link stays up; no state loss |
| `brightness` (7) | POST | `{"value":<int 0-100>}` | bare 200 | Panel brightness = value×2.5 (Android 0-250 scale) |
| `sysinfoDisplay` (8) | POST | `{"items":["<metric>",...]}` | bare 200 | Replace overlay metric list (flat single-screen form) |
| `fanLCD` (9) | POST | `{"speed":"<str>","mode":"<str>"}` | bare 200 | Update only `speed`+`mode` in-memory (both keys mandatory) |
| `fanLCDSet` (10) | POST | full `FanLCD` object | bare 200 | Replace whole in-memory fan curve (applied on next `all`) |
| `temperature` (11) | POST | `{"value":"Celsius"\|"Fahrenheit"}` | bare 200 | Display temp unit (device converts; telemetry stays °C) |
| `spec` (12) | POST | `{"cpu":"<name>","gpu":"<name>"}` | bare 200 | Set CPU/GPU badge title strings |
| `rotate` (13) | POST | `{"degree":<int>}` | bare 200 | `SystemProperties.set("persist.vendor.orientation",<degree>)`; effect on display re-init |
| `config` (14) | POST | bulk object (see Display config) | bare 200 (both processes) | Bulk apply: temp unit, block-screen enable/sleep/brightness/id/fanLCD, spec, turboPump |
| `waterfallMode` (15) | POST | — | bare 200 | **DEAD**: recognized (`c=15`) but no `case 15` in action switch → `default:return`. Skip entirely |

Discovery is **not** a HomeUI cmdType — it is `conn`, handled entirely inside SerialService (never reaches HomeUI). See Device Controls §spec.

---

## sysinfo "all" schema (`PcInfo`)

Parsed by plain `new Gson()` into `PcInfo` (`MRM:80-84` STATE, `MRM:234-236` POST). **Unknown keys are silently dropped; missing keys leave Java defaults (0/null).** Client always sends temperatures in **°C** — device converts for display (and, buggily, for the fan curve — see Fan §Gotcha).

Top-level (`PcInfo.java:7-14`): `cpu`, `disk`, `fans[]`, `gpu`, `memory`, `motherboard`, `network`, `timestamp`.

| JSON path | Type | Unit | Shown? | Notes / evidence |
|---|---|---|---|---|
| `timestamp` | long | epoch **ms** | clock only | Sets device clock if drift >300 s (`MainActivity.full.java:125-129`) |
| `cpu.temperature` | float | °C | **yes** | Feeds display AND fan curve (`PcCpu.java:12,31-36`; `badcode:131`) |
| `cpu.load` | int | % | **yes** | `PcCpu.java:9` |
| `cpu.speedAverage` | int | **MHz** | **yes** | `PcCpu.java:11` |
| `cpu.voltage` | float | V | **yes** | Shown verbatim e.g. "1.25" (`PcCpu.java:13`) |
| `cpu.fanAverage` | int | — | no | Parsed, never read (`PcCpu.java:8`) |
| `cpu.power` | int | — | no | Parsed, never read (`PcCpu.java:10`) |
| `gpu.temperature` | **String** | °C | **yes** | `Float.parseFloat`; **NPE-crash if `gpu` present but temperature null/absent** — always send it, number or numeric string (`PcGpu.java:12,39-44`) |
| `gpu.load` | int | % | **yes** | `PcGpu.java:9` |
| `gpu.speed` | int | **MHz** | **yes** | `PcGpu.java:11` |
| `gpu.voltage` | float | V | **yes** | `PcGpu.java:13` |
| `gpu.fan`, `gpu.power` | int | — | no | Never read (`PcGpu.java:8,10`) |
| `memory.speed` | int | **MHz** | **yes** | Label "Memory Frequency" (`PcMem.java:9`) |
| `memory.load` | long | % | **yes** | Label "Memory Utilization" (`PcMem.java:8`) |
| `memory.total`, `memory.used`, `memory.temperature` | long,long,float | — | no | Never read (`PcMem.java:10-12`) |
| `disk.temperature` | int | °C | **yes** | Shown as **"Hard Disk Temperature"** in `tv_mem_temp` (misleading id) (`Disk.java:11`) |
| `disk.load/used/total/activity/readSpeed/writeSpeed` | int×6 | — | no | Never read (`Disk.java:8-14`) |
| `motherboard.temperature` | float | °C | **yes** | Label "Motherboard Temperature" (`PcMd.java:8`) |
| `network.upload`, `network.download` | int,int | — | no | Entire `network` discarded (`badcode:121`) |
| `fans[]` = `{id,name,value,max}` | array | — | no | Entire `fans[]` discarded (`Fan.java:5-8`; `badcode:124`) |

Rendered fields mirror to split-screen `tv2*` views. On-screen clock is a `TextClock` (`HH:mm` / `yyyy/MM/dd`) after the timestamp sync. Wire keys not in the POJO (e.g. `cpu.usage`) are safely ignored.

---

## Status reply body

Two shapes, both `version="1"`, `cmdType="200"`, `AckNumber = seq+1`. Only `all` gets a body; everything else is the bare SerialService 200.

**STATE all → full body** (`MRM:88-108`):
```json
{"status":{"fanLCD":"<speed file>","turboPump":"<rpm file>"},"warning":"[{\"description\":\"No ERROR\",\"type\":\"Fan LCD\"}]","availableStorage":123456789}
```

**POST all → reduced body** (`MRM:238-252`):
```json
{"status":{"fanLCD":"<speed file>"}}
```

Field sources:
- `status.fanLCD` — first line of `/sys/bus/platform/drivers/lcd_fan/speed` (string; `""` on error). `MRM:39,93`.
- `status.turboPump` — first line of `/sys/bus/i2c/drivers/aio_cooler/rpm` (string). STATE-only. `MRM:40,94`.
- `warning` — **hardcoded constant, double-encoded JSON string**. Always exactly `[{"description":"No ERROR","type":"Fan LCD"}]`. Built from `new Warning("Fan LCD","No ERROR")` where constructor maps `type=arg1, description=arg2` (`Warning.java:5-11`), Gson serializes in declaration order `description` then `type` (`MRM:96-98`). **There is no error-detection path anywhere in HomeUI — treat as constant noise.** (Critic resolved: the other two reports' key/order variants are wrong.)
- `availableStorage` — bytes free on `/sdcard`: `StatFs.getAvailableBlocksLong() * getBlockSizeLong()` (long JSON number) (`MRM:491-494`). STATE-only.

---

## Display configuration

Handler `waterBlockScreenId` (`MRM:283-289`, case 4): entire body is a `ScreenConfig`, rendered at `MainActivity.full.java:236-422` (msg 105). Top-level branch is on **`id`**, not `screenMode`:
- `id == "Customization"` → user layout (Full Screen or Screen Splitting).
- `id != "Customization"` and non-empty → **preset video** (Mode C below).
- Empty `id` → rejected, return (`lines 239-241`).

`ScreenConfig` POJO (`entity/ScreenConfig.java:7-14`): `Type` (Gson key capital `"Type"`, always sent null — omit), `id`, `media` (`ArrayList<String>`, getter typo `getMeida`), `playMode`, `ratio`, `screenMode`, `settings` (Object), `sysinfoDisplay` (Object). `settings`/`sysinfoDisplay` shape-shift by screenMode.

### Mode A — Full Screen (1 region)
`settings` = single `PmSetting` object; `sysinfoDisplay` = **flat string array**; `ratio` present. Verbatim wire capture (`ImportantInfo3.txt:1918`):
```json
{"id":"Customization","screenMode":"Full Screen","playMode":"Loop","ratio":"2:1","media":["2025-11-29_01-19-22-612.gif","2025-11-27_07-07-46-308.gif","2025-11-27_07-00-23-055.png","2025-11-28_15-10-46-857.png"],"settings":{"color":"#dcdcdc","align":"Left","filter":{"value":"Rain","opacity":100},"badges":[]},"sysinfoDisplay":[]}
```
`ratio` sets the media container size (`MainActivity.full.java:257-313`):

| ratio | width × height |
|---|---|
| `"1:1"` | 1120 × 1080 |
| `"2:1"` | native `screen_width` × `screen_height` |
| `"3:2"` | 1620 × 1080 |
| `"4:3"` | 1440 × 1080 |
| `"16:9"` | 1920 × 1080 |

Any other ratio → default keeps native.

### Mode B — Screen Splitting (2 regions)
`settings` = **array of 2** `PmSetting`; `sysinfoDisplay` = **array of 2 arrays**; **no `ratio`**. `playMode` is forced to internal `"single"` (no playlist advance). `media[0]`→region 1, `media[1]`→region 2. Region 2 filter always uses `_1_1` drawable variants. Verbatim wire capture (`ImportantInfo2.txt:239`):
```json
{"id":"Customization","screenMode":"Screen Splitting","playMode":"Single","media":["2025-12-02_20-57-01-835.png"],"settings":[{"color":"#000000","align":"Left","filter":{"value":null,"opacity":100},"badges":[]},{"color":"#000000","align":"Center","filter":{"value":null,"opacity":100},"badges":["CPU Badge","GPU Badge"]}],"sysinfoDisplay":[[],["CPU Temperature","GPU Temperature"]]}
```
Screen mode string test: only `"Full Screen"` and `"Screen Splitting"` recognized (`lines 251,1257`); anything else inside Customization → treated as Screen Splitting.

### Mode C — Preset (`id != "Customization"`)
`media`/`playMode` ignored; plays built-in video (`MainActivity.full.java:391-421`):
```java
setLayout1Path(basevideoPath + config.getId().split(":")[1].trim().replace(" ","_") + ".mp4");
```
`basevideoPath = "/system/media/video/"`. `id` must contain `:` — e.g. `"x: Neon Wave"` → `/system/media/video/Neon_Wave.mp4`. `settings` = single object, `sysinfoDisplay` = flat array. `[NEEDS-HW]` no preset captured on the wire; exact preset id strings unknown.

### playMode enum (`MainActivity.full.java:197-215`)
- `"Single"` → always `media[0]`.
- `"Loop"` → `showNum++`, wraps at `media.size()`.
- `"Shuffle"` → `random.nextInt(media.size())`.
- Image advance timer = **5000 ms** (`mainHandler.sendEmptyMessageDelayed(102,5000L)`); video advances on `onCompletion`.
- Lowercase `"single"` is an internal sentinel (Screen Splitting), not a wire value. No "Slideshow".
- Media path prefix: `sdcard/pcMedia/`. `.mp4` → VideoView; png/gif → Glide image.

### filter (`setAnimation1`, `MainActivity.java:160-177`)
- `filter.value`: device discriminates only `"Smoke"` → smoke drawable; any other non-empty → **rain** drawable; `null`/`""` → filter cleared (`setImageResource(0)`). Wire-observed: `"Rain"`, `"Smoke"`, `null`. `"Vapor"` is a dead PC-app option (renders as rain). The `/system/media/anim/*.webp` path is log-only; assets are APK drawables (`rain.webp`, `rain_1_1.webp`, `smoke.webp`, `smoke_1_1.webp`).
- `filter.opacity`: int 0-100 → alpha = opacity/100.0, animated over 300 ms.

### PmSetting (`entity/PmSetting.java`)
- `color`: `"#RRGGBB"` hex → `Color.parseColor`. Observed `"#dcdcdc"`, `"#000000"`.
- `align`: `"Left"`(gravity 19) / `"Center"`(17) / `"Right"`(21) (`MainActivity.java:501-512`). Other → no change.
- `badges`: only `"CPU Badge"` and `"GPU Badge"` (`MainActivity.java:453-461`). Background auto-colored by vendor substring (see Enum ref).
- `filter`: `{value, opacity}` as above.
- `position`: **DEAD** — `getPosition()` has zero call sites. Omit or send anything.
- `sysinfoDisplay` item strings: the 13-value closed set (see Enum ref).

### PmSetting via config
`config` cmdType (case 14) embeds the same ScreenConfig at `waterBlockScreen.id` (`MRM:383`). See Fan/Device sections for the full `config` body.

### waterfallMode / rotate
- **`waterfallMode` is dead** — recognized (`c=15`) but no `case 15` in the action switch; `doWaterfallMode`/`changeWaterModelPosition` have zero call sites; `MSG_WATERFALLMODE_CHANGE=116` has no handler case. **Skip.**
- `rotate` (`MRM:365-374`): `{"degree":<int>}` → `SystemProperties.set("persist.vendor.orientation","<degree>")`. No in-app change; consumed by vendor display stack on re-init/reboot. No range validation. `[NEEDS-HW]` valid degree values and whether reboot is required.

---

## Fan & pump control

### FanLCD POJO (`FanLCD.java:6-10`)
```java
int fixedMode; String mode; ArrayList<ArrayList<Integer>> smartMode; String speed;
```

### `fanLCDSet` (case 10) — replace whole curve
Entire body = a `FanLCD` (Gson, `MRM:338-341`). **Nothing is written to hardware here** — it only replaces the in-memory curve; it takes effect on the next `all` push.
```json
{"speed":"Mid Speed","fixedMode":45,"mode":"Smart Mode","smartMode":[[0,0],[10,20],[30,30],[50,40],[65,55],[80,70],[90,100]]}
```
`smartMode` = `ArrayList<ArrayList<Integer>>`, each inner list a 2-element `[temp,duty%]` point sorted ascending by temp. Point count not fixed (device iterates `length`). Built-in default when `smartMode` null (`MA:113`): `{{0,0},{10,20},{30,30},{50,40},{65,55},{80,70},{90,100}}`. Duty axis 0-100 (percent). Temp axis is in whatever unit `temperature` selected (see Gotcha).

### `fanLCD` (case 9) — subset update
```json
{"speed":"<string>","mode":"<string>"}
```
Both keys **mandatory** — `getString` throws if either absent, and then nothing is applied (`MRM:327-328`). Cannot carry `fixedMode`/`smartMode`. `[NEEDS-HW]` note: dereferences `this.fanLCD` directly (not `getFanLCD()`), NPE-prone if sent before any `fanLCDSet`/`config`/`all`.

### `config` (case 14) — bulk. `fanLCD` nested inside `waterBlockScreen`; `turboPump` top-level.
Union of what both processes parse:
```json
{
  "temperature": "Celsius",
  "waterBlockScreen": {
    "enable": true,
    "displayInSleep": true,
    "brightness": 40,
    "id": { /* ScreenConfig object */ },
    "fanLCD": { "speed":"Mid Speed","fixedMode":45,"mode":"Smart Mode","smartMode":[[0,0],[10,20],[30,30],[50,40],[65,55],[80,70],[90,100]] }
  },
  "spec": { "cpu": "<name>", "gpu": "<name>" },
  "turboPump": { "enable": true, "value": 65 }
}
```
- HomeUI (`MRM:375-400`) parses: `temperature`, `waterBlockScreen.{enable,displayInSleep,brightness,id,fanLCD}`, `spec.{cpu,gpu}`. `brightness` scaled ×2.5 → panel (`MRM:480`). All in one try — a missing key aborts remaining assignments in that process.
- SerialService (`SMRH:341-355`) parses `turboPump.{enable,value}` → sysfs writes, then bare 200. HomeUI never touches pump control.

### mode / speed / fixedMode semantics
- `mode`: only `"Smart Mode"` is tested (`MA:210`) → curve. Any other string (PC sends `"Fixed Mode"`) → fixed-duty branch writes `fixedMode` verbatim to fan sysfs. Effectively boolean.
- `fixedMode`: int fan duty 0-100 written directly in non-Smart mode. Default 45.
- `speed`: keys `"Low Speed"`/`"Mid Speed"`/`"High Speed"`/`"Full speed"` (note lowercase `s` in last) → `speedMap` 0.4/0.6/0.8/1.0, but **`speedMap` is never read** — zero hardware effect, round-tripped only.
- Defaults when nothing received (`MRM:496-505`): `mode="Smart Mode"`, `speed="Mid Speed"`, `fixedMode=45`, `smartMode=null` (→ built-in table).

### Curve algorithm & write trigger
Write happens on **every `all` frame**: `setFanSpeed(cpu.getTemperature())` (`MA.full:131`). Interpolation (`MA:206-256`, verified against raw dalvik because jadx's Java restructuring is wrong):

For input temp `t`, iterate consecutive pairs `(cfg[k],cfg[k+1])` for `k = 0 .. len-3`:
- `p1[0]==t` → exact hit, duty = `p1[1]`.
- `p1[0] < t && p2[0] > t` → bracket: linear interp `jisuan1=(t-t1)/(t2-t1)`, `duty = d1 + (d2-d1)*jisuan1` (all float).
- else → no-op.

Result truncated to int, written to `/sys/bus/platform/drivers/lcd_fan/speed`.

**Two real quirks (bytecode-confirmed — replicate or knowingly fix in Rust):**
1. Loop bound `k <= len-3` → **the last curve point is never used**. Default 7-point table: `{90,100}` is dead; effective ceiling bracket is `{80,70}`.
2. If `t >=` the second-to-last point's temp (and no earlier exact hit) → no bracket → `duty = 0` written at hottest temps. Default table: `t=85` writes `0`.

**Gotcha:** `PcCpu.getTemperature()` returns the **unit-converted** value (°F if `temp_unit=="Fahrenheit"`) and that same value feeds the curve. So if you set Fahrenheit, `smartMode` temp points must be in °F. Keep telemetry °C + unit Celsius to avoid this.

### turboPump (SerialService only)
Standalone cmdType `turboPump` (`SMRH:325-340`) or nested in `config` (`SMRH:341-355`). Body `{"enable":<bool>,"value":<int>}`:
- `control_source` ← `1` if enable else `0` (host-PWM vs auto).
- `pwm` ← `value` (pump duty when enabled).
- `enable` persisted to SharedPreferences key `"key_turbo"` (`SpUtils.KEY_TURBO`).
- Boot restore (`SMRH:107-137`): writes `control_source=0`, sleeps 800 ms, reads `rpm`; if `rpm>=3200` pump is turbo-capable → if saved `key_turbo` then `control_source=1;pwm=65` else `0`; retries up to 4× at 1 s. Turbo attribute only advertised in `conn` when `rpm>=3200`.
- Feedback: only via `STATE all` reply `status.turboPump` (raw rpm file). No unsolicited pump push.

---

## Device controls

**brightness** (case 7, `MRM:303-310`): `{"value":<0-100>}` → `onDoBrightness((int)(value*2.5))` → Android `Settings.System.putInt("screen_brightness",…)` + window attr + `screen_brightness_mode=0` (manual) (`Util.java:21-39`). **NOT sysfs.** Persisted to SharedPreferences file `"tks_home_share"` key `"key_brightness"` (default 204). PC 0-100 → stored/applied 0-250.

**power** (case 1, `MRM:256-264`): `{"event":<str>}`. 5 strings: `"suspend"`/`"shutdown"`/`"lock-screen"` (all identical screen-off path) and `"resume"`/`"unlock-screen"` (screen-on). Screen-off consults `displayInSleep`: true→`showStandby()` (plays `/system/media/video/standby.mp4`), false→brightness 0. Never actually powers off Android. Not persisted. Other strings ignored.

**waterBlockScreen** (case 2, `MRM:265-273`): `{"enable":<bool>}` → pure display toggle (`MainActivity.full.java:429-446`). true→restore `saveBright`; false→save then brightness 0. **Does NOT consult `displayInSleep`** — `false` is always black, never standby video (contrast power/disconn). Also driven via `config.waterBlockScreen.enable` (`MRM:396`). *(This is the 16th cmdType the critic found missing from the misc report.)*

**displayInSleep** (case 3, `MRM:274-282`): `{"enable":<bool>}`. Field default `true` (`MRM:41`). Consulted only in power-suspend and disconnect screen-off paths: true→standby video, false→black. Also set via `config.waterBlockScreen.displayInSleep`. **Not persisted** (resets true on restart).

**rotate** (case 13): see Display config.

**preset** (case 5, `MRM:290-296`): body = `ScreenConfig`, but only `sysinfoDisplay`+`settings` applied (both Full-Screen shape) merged onto current config — media/playMode/ratio/screenMode untouched (`MainActivity.full.java:526-550`). Overlay-style live update. Not persisted.

**spec** (case 12, `MRM:351-364`): `{"cpu":"<name>","gpu":"<name>"}` → sets badge titles `cpuName`/`gpuName` (and `2` variants). Also via `config.spec`. Reply: bare 200 (device sends nothing back). **This is a PC→device push, not a query.**
> Discovery is cmdType **`conn`**, handled entirely inside SerialService (`SMRH:192-231`), never reaching HomeUI. Reply body:
> ```json
> {"attribute":["Status","Water Block Screen","Fan LCD|rw"/*,"Turbo Pump" if rpm>=3200*/],"OS":"Android","productId":"cm01","version":{"app":"<HomeUI versionName>","firmware":"<Build.DISPLAY>","hardware":"<ro.hwversion>"},"sn":"<ro.serialno>"}
> ```
> A Rust client should `POST conn` for discovery and `POST spec` to label badges.

**disconn** (case 6, `MRM:297-302`): no body. Same as power-suspend (save brightness, then `displayInSleep?showStandby():brightness0`). Link stays up, no state closed. Also fired by the 60 s watchdog. Recovery: next frame → `onReConnect` restores brightness + re-applies saved `config`. Not persisted.

**temperature** (case 11, `MRM:342-350`): `{"value":"Celsius"|"Fahrenheit"}`. Conversion is on-device in every entity getter: `(int)((temp*1.8)+32.0)` for Fahrenheit (`PcCpu.java:33` etc.). Client always sends °C telemetry. Unit glyphs flip `℃`/`℉` on next `all` refresh. Also via `config.temperature`. Not persisted (static field).

**sysinfoDisplay** (case 8, `MRM:311-323`): `{"items":["<metric>",...]}` (flat single-screen array) → replaces overlay item list only; colors/badges/align/media untouched, and does NOT write back into `config` (so a later resume/reconnect reverts it). Metric strings = the 13-value closed set. Not persisted.

SharedPreferences summary: HomeUI file `"tks_home_share"`, only key written by these commands is `"key_brightness"` (int 0-250, default 204). SerialService separately persists `"key_turbo"` (different APK/prefs).

---

## Enum reference (consolidated — "recognized" = literal changes behavior; "pass-through" = round-tripped only)

**cmdType map (16):** `all`(0), `power`(1), `waterBlockScreen`(2), `displayInSleep`(3), `waterBlockScreenId`(4), `preset`(5), `disconn`(6), `brightness`(7), `sysinfoDisplay`(8), `fanLCD`(9), `fanLCDSet`(10), `temperature`(11), `spec`(12), `rotate`(13), `config`(14), `waterfallMode`(15 = dead).

**screenMode** (`id=="Customization"`): `"Full Screen"`, `"Screen Splitting"`. Other → Screen Splitting. No waterfall.

**playMode:** `"Single"`, `"Shuffle"`, `"Loop"`. Internal `"single"` = no-advance sentinel. No "Slideshow". Empty/null → no playback.

**ratio** (Full Screen only): `"1:1"`(1120×1080), `"2:1"`(native), `"3:2"`(1620×1080), `"4:3"`(1440×1080), `"16:9"`(1920×1080). Other → native.

**filter.value:** `"Smoke"`→smoke; any other non-empty→rain; `null`/`""`→cleared. `"Vapor"` (dead PC option)→rain.

**filter.opacity:** int 0-100.

**PmSetting.align:** `"Left"`, `"Center"`, `"Right"`. Other → no change.

**PmSetting.position:** DEAD (never read).

**PmSetting.color:** `"#RRGGBB"` hex.

**badges:** `"CPU Badge"`, `"GPU Badge"`. Background auto-color by vendor substring: contains `"Intel"`→`name_bg_blue`, contains `"NVIDIA"`→`name_bg_green`, else `name_bg_red` (AMD = red) (`MainActivity.java:465-473`).

**FanLCD.mode:** `"Smart Mode"` (curve) vs anything else (fixed duty). PC sends `"Fixed Mode"`.

**FanLCD.speed** (pass-through, no HW effect): `"Low Speed"`, `"Mid Speed"`, `"High Speed"`, `"Full speed"` (lowercase `s`).

**temperature.value / temp_unit:** `"Celsius"`, `"Fahrenheit"` (anything ≠ `"Fahrenheit"` → Celsius). Client always sends °C telemetry.

**power.event:** `"suspend"`, `"shutdown"`, `"lock-screen"` (screen-off); `"resume"`, `"unlock-screen"` (screen-on). Other → ignored.

**sysinfoDisplay metrics (13, closed, case-sensitive `.equals`):**
`"CPU Temperature"`, `"GPU Temperature"`, `"CPU Frequency"`, `"GPU Frequency"`, `"CPU Usage"`, `"GPU Usage"`, `"CPU Voltage"`, `"GPU Voltage"`, `"Motherboard Temperature"`, `"Hard Disk Temperature"` (binds **disk.temperature**, misleading `tv_mem_temp` id), `"Memory Frequency"`, `"Memory Utilization"`, `"Date&Time"`.
Field→source: CPU Temp=`cpu.temperature`, GPU Temp=`gpu.temperature`, CPU Freq=`cpu.speedAverage`, GPU Freq=`gpu.speed`, CPU Usage=`cpu.load`, GPU Usage=`gpu.load`, CPU Voltage=`cpu.voltage`, GPU Voltage=`gpu.voltage`, Motherboard Temp=`motherboard.temperature`, Hard Disk Temp=`disk.temperature`, Memory Freq=`memory.speed`, Memory Util=`memory.load`, Date&Time=device clock.
(`defultConfig={"Cpu Usage","Gpu Usage","Date&Time"}` at `MainActivity.java:84` is a dead fallback; lowercase spellings never match.)

**warning body (constant):** `[{"description":"No ERROR","type":"Fan LCD"}]` (double-encoded string).

**conn attributes:** `"Status"`, `"Water Block Screen"`, `"Fan LCD|rw"`, plus `"Turbo Pump"` iff pump rpm≥3200. `"OS":"Android"`, `"productId":"cm01"`.

---

## Open questions / needs-hardware-verification

1. **Preset id strings** — Mode C (`id != "Customization"`) splits on `:` and maps `"x: Neon Wave"`→`/system/media/video/Neon_Wave.mp4`. Exact preset names / available `.mp4` files not captured on the wire. `[NEEDS-HW]` enumerate `/system/media/video/`.
2. **rotate degree values** — no validation, no wire capture; unclear which degrees are valid and whether a reboot/display re-init is required for `persist.vendor.orientation` to apply. `[NEEDS-HW]`
3. **`fanLCD` NPE risk** — case 9 dereferences `this.fanLCD` directly; verify sending `fanLCD` before any `fanLCDSet`/`config`/`all` doesn't crash HomeUI. Safe pattern: send `fanLCDSet` (or `all`) first. `[NEEDS-HW]`
4. **Fan curve quirks** — confirm on hardware that duty 0 is actually written at the hottest temps (quirk 2) and the last point is dead (quirk 1). Decide whether the Rust client sends a curve that compensates (e.g. add a sentinel trailing point above max temp so the real ceiling is the second-to-last).
5. **PC-app enum supersets** — HomeUI's decompiled tree has no app-level `strings.xml`/`arrays.xml`/`R.java`, so the full option lists the Windows client offers (extra filters, preset names, etc.) are unresolvable here. Functionally moot: any unrecognized value hits a documented default. `[NEEDS-HW / needs Windows client]`
6. **Turbo capability gate** — `conn` advertises `"Turbo Pump"` and boot-restore acts only when pump `rpm>=3200`. Verify this device's pump reports ≥3200 before relying on turbo. `[NEEDS-HW]`
7. **`config` partial-abort behavior** — a missing key aborts remaining assignments within a process's single try-block. Client should always send the complete `config` object.
8. **Nothing hardware-tested yet** — per project memory, the entire protocol is mapped from decompilation + logcat captures; no command has been round-tripped against the real device. Treat all "effect" claims as decompilation-derived until confirmed on hardware.

Primary source files: `MsgReceiverManager.java`, `MainActivity.full.java` (complete `handleMessage`), `SerialMsgReceiverHandler.java` (SerialService), `entity/*.java`, wire captures `ImportantInfo2.txt` / `ImportantInfo3.txt` — all under `/home/shadowbroker/tryx_panorama_linux/aio-reverse/`.

---

# Appendix: Completeness / gaps analysis

Verification complete. Here is the completeness-critic pass.

---

# COMPLETENESS CRITIC — TRYX Panorama HomeUI reports

## 1. MISSING / UNRESOLVED

### 1a. A whole cmdType is undocumented: `waterBlockScreen` (the bare one)
None of the five reports cover `waterBlockScreen` — the misc-commands report is titled "The 8 remaining cmdTypes" and lists brightness/power/displayInSleep/preset/spec/disconn/temperature/sysinfoDisplay, silently dropping the 16th. It is a real, live handler, distinct from `waterBlockScreenId`:

- Dispatch: `MsgReceiverManager.java:214-215` `cmdType.equals("waterBlockScreen")` → `c = 2` (line 216).
- Handler `case 2` (`MsgReceiverManager.java:265-273`): body `{"enable": <bool>}` → `doBlockScreen(new JSONObject(...).getBoolean("enable"))`.
- `doBlockScreen` (`MsgReceiverManager.java:461-464`) → `onBlockScreen(z)` → `MainActivity.full.java:1157-1163` posts msg `107`.
- Effect, `case 107` (`MainActivity.full.java:429-446`): `enable=true` → `isScreenOn=true; Util.changeAppBrightness(saveBright)`; `enable=false` → save current brightness then `Util.changeAppBrightness(0)`. **It is a pure display on/off toggle.** Note it does NOT consult `displayInSleep`, so `false` always goes to brightness-0 (black), never the standby video — unlike `power`/`disconn`.
- Also driven in bulk: `config` calls `doBlockScreen(z)` from `waterBlockScreen.enable` (`MsgReceiverManager.java:396`).
- Reply: none from HomeUI; SerialService sends the generic empty `200`.
- Body JSON (definitive): `{"enable": true}`.

### 1b. `sysinfoDisplay` metric set is complete but was reported at 13 vs "partial" ambiguity
All three reports that touch it (sysinfo-render §3, screen-config §4, misc-commands §8) list the same 13 strings. That set is exhaustive — `showInfo`/`showInfo2` branch on exactly these and nothing else. Resolved: 13, closed set (table below).

### 1c. Full PC-app enum supersets are NOT extractable here (and don't need to be)
The device only *discriminates* on a few literals per field; every other value falls to a default. The complete option list the PC app offers (e.g. a `"Vapor"` filter — dead constant `animVaporPath` at `MainActivity.full.java` filter code, renders as rain) lives in the **Windows client**, not HomeUI. I confirmed HomeUI's decompiled tree carries **no app-level `R.java`, `arrays.xml`, or `strings.xml`** (only `androidx/*/R.java` exist; the `/tmp/homeui-*/resources/res/values*` dirs the reports referenced are not present in this session). Conclusion: the device-recognized enums below are complete and authoritative for the Rust client; the PC superset is unresolvable from these artifacts but functionally irrelevant (unknown → default branch).

### 1d. Minor unresolved units (parsed-but-never-read — unit is unknowable and irrelevant)
`cpu.fanAverage`, `cpu.power`, `gpu.fan`, `gpu.power`, `memory.total/used/temperature`, all of `disk.*` except `temperature`, all of `network`, all of `fans[]`. sysinfo-render correctly flags these as discarded; there is no display path to infer a unit from. Rust client may send anything or omit.

---

## 2. CONTRADICTIONS

### 2a. The `warning` string shape — three different answers across reports; only one is correct
- sysinfo-render §0: `"[{\"name\":\"Fan LCD\",\"value\":\"No ERROR\"}]"` — **WRONG keys.**
- fan-pump-config §5: `"[{\"type\":\"Fan LCD\",\"description\":\"No ERROR\"}]"` — right keys, **wrong order.**
- status-reply §2: `"[{\"description\":\"No ERROR\",\"type\":\"Fan LCD\"}]"` — **CORRECT.**

Ground truth (`entity/Warning.java:5-6`): fields declared `description` then `type`; constructor `Warning(str,str2){ this.type=str; this.description=str2; }` (lines 8-11), invoked `new Warning("Fan LCD","No ERROR")` (`MsgReceiverManager.java:97`) → `type="Fan LCD"`, `description="No ERROR"`. Plain `new Gson()` serializes in declaration order → `{"description":"No ERROR","type":"Fan LCD"}`, embedded as a double-encoded string via `jSONObject.put("warning", ...)` (line 98).

**Definitive STATE-all reply body:**
```json
{"status":{"fanLCD":"<speed file>","turboPump":"<rpm file>"},"warning":"[{\"description\":\"No ERROR\",\"type\":\"Fan LCD\"}]","availableStorage":<long bytes>}
```

### 2b. No contradiction with known SerialService behavior — but one point worth pinning
The ">=2 headers or NPE" rule and the SerialService/HomeUI split (SerialService: conn/transport/transported/mediaDelete/turboPump/config/all-ack-suppression; HomeUI: render/control) are consistent across reports. Note the reports correctly capture that `config` and `all` are handled **by both** processes in parallel (SerialService writes turboPump sysfs + acks; HomeUI does the render/brightness/fanLCD side). No report contradicts this.

### 2c. Non-contradiction to note: `waterfallMode` truly dead
Confirmed: `waterfallMode` → `c = 15` (`MsgReceiverManager.java:166-167`), but the action switch has only `case 0..14` (verified: labels end at `case 14` line 375, then `default` line 405). Falls through to `default: return`. screen-config's "dead code" verdict stands.

---

## 3. DEFINITIVE CONSOLIDATED ENUM TABLES

"Device-recognized" = the literal changes behavior. "Pass-through" = stored/logged/round-tripped only, no branch. Send only recognized values to be safe; treat everything else as the documented default.

**screenMode** (`MainActivity.full.java:251,1257`) — recognized: `"Full Screen"`, `"Screen Splitting"`. Anything-else inside `id=="Customization"` → treated as Screen Splitting. No `"waterfall"` mode exists.

**playMode** (`MainActivity.full.java:197,201,202`) — recognized: `"Single"`, `"Shuffle"`, `"Loop"`. Internal sentinel `"single"` (lowercase) is forced by Screen Splitting (line 346) and means "no playlist advance". No `"Slideshow"`. Empty/null → no playback.

**ratio** (Full Screen only; `MainActivity.full.java:106,259,268,275,282`) — recognized: `"1:1"`(1120×1080), `"2:1"`(native screen_width×screen_height), `"3:2"`(1620×1080), `"4:3"`(1440×1080), `"16:9"`(1920×1080). Any other → default branch keeps native (c stays 1).

**filter.value** (`MainActivity.full.java:624,650`) — device discriminates only `"Smoke"` (→ smoke drawable). Everything else non-empty → rain drawable. `null`/`""` → filter cleared (`setImageResource(0)`). Wire-observed: `"Rain"`, `"Smoke"`, `null`. `"Vapor"` is a dead PC-app option → renders as rain.

**PmSetting.align** (`MainActivity.java:501-512`) — recognized: `"Left"`(gravity 19), `"Center"`(17), `"Right"`(21). Any other → falls through (no gravity change).

**PmSetting.position** (`entity/PmSetting.java:11-19`) — **DEAD**. `getPosition()` has zero call sites. Field parsed, never used. Rust client may omit or send anything.

**FanLCD.mode** (`MainActivity.java:210`) — only `"Smart Mode"` is tested (→ curve interpolation). Any other string (PC sends `"Fixed Mode"`) → fixed-duty branch writes `fixedMode`. So it is effectively boolean: `"Smart Mode"` vs not.

**FanLCD.speed** (`MainActivity.java:147-150`) — keys `"Low Speed"`, `"Mid Speed"`, `"High Speed"`, `"Full speed"` (note lowercase `s` in the last). Mapped 0.4/0.6/0.8/1.0 in `speedMap` which is **never read** — zero hardware effect; round-tripped only.

**temp_unit / temperature.value** (`entity/PcCpu.java:32`; `MainActivity.full.java:169,180`) — exactly two: `"Celsius"`, `"Fahrenheit"`. Anything not equal to `"Fahrenheit"` → Celsius (no conversion). Client always sends °C telemetry; device converts for display AND for the fan curve.

**badges** (`MainActivity.java:453-461`) — exactly two: `"CPU Badge"`, `"GPU Badge"`. (Background color auto-picked from vendor substring: `Intel`→blue, `NVIDIA`→green, else red.)

**power.event** (`MainActivity.full.java:450,468`) — five: `"suspend"`, `"shutdown"`, `"lock-screen"` (all = screen-off) and `"resume"`, `"unlock-screen"` (screen-on). Anything else ignored.

**sysinfoDisplay metric strings** (13, closed set; `MainActivity.java:585-611`) — case-sensitive `.equals`:
`"CPU Temperature"`, `"GPU Temperature"`, `"CPU Frequency"`, `"GPU Frequency"`, `"CPU Usage"`, `"GPU Usage"`, `"CPU Voltage"`, `"GPU Voltage"`, `"Motherboard Temperature"`, `"Hard Disk Temperature"` (binds **disk.temperature**, misleading `tv_mem_temp` id), `"Memory Frequency"`, `"Memory Utilization"`, `"Date&Time"`. The `defultConfig={"Cpu Usage","Gpu Usage","Date&Time"}` (MainActivity.java:84) is a dead fallback; its lowercase spellings would never match.

**Full HomeUI cmdType map (16, definitive)** — `all`(0), `power`(1), **`waterBlockScreen`(2)**, `displayInSleep`(3), `waterBlockScreenId`(4), `preset`(5), `disconn`(6), `brightness`(7), `sysinfoDisplay`(8=`\b`), `fanLCD`(9=`\t`), `fanLCDSet`(10=`\n`), `temperature`(11), `spec`(12=`\f`), `rotate`(13=`\r`), `config`(14), `waterfallMode`(15=dead, no case).

---

## 4. FOLLOW-UP GREPS RUN (with results)

- `entity/Warning.java` full read → fields `description`(L5), `type`(L6); constructor maps `type=arg1, description=arg2` → resolved contradiction 2a. **CORRECT form: `[{"description":"No ERROR","type":"Fan LCD"}]`.**
- `grep 'case ' MsgReceiverManager.java` → exposed **`case 2` / `waterBlockScreen`** never covered by any report (gap 1a). Read `MsgReceiverManager.java:265-273` and `MainActivity.full.java:1157-1163,429-446` → full behavior recovered.
- `grep -E '"1:1"|"2:1"|...' MainActivity.full.java` → ratio set = {1:1,2:1,3:2,4:3,16:9}, complete.
- `grep 'playMode.equals' MainActivity.full.java` → {Single,Shuffle,Loop} + internal `single`; no Slideshow.
- `grep 'getValue().equals' MainActivity.full.java` → only `"Smoke"` discriminated (2 sites, full-screen + region-2), confirms rain-default.
- `grep 'getScreenMode().equals' MainActivity.full.java` → only `"Full Screen"` / `"Screen Splitting"`.
- `find … -name arrays.xml -o -name strings.xml -o -name R.java` across all decompiled/tmp trees → **no app-level resource tables present** (only `androidx/*/R.java`); the `/tmp/homeui-*/resources` dirs the reports cited are gone this session → gap 1c is unresolvable from current artifacts but functionally moot.
- Second-switch label scan → confirmed labels stop at `case 14`; `waterfallMode`(15) is dead (2c).

**Net:** reports are accurate except (a) they omit the `waterBlockScreen` on/off cmdType entirely, and (b) two of three `warning`-body renderings are wrong — use `[{"description":"No ERROR","type":"Fan LCD"}]`. All enum tables above are complete for the device's decision logic.

Key files: `/home/shadowbroker/tryx_panorama_linux/aio-reverse/homeui-decompiled/sources/com/baiyi/homeui/tkcfanhomeui/manage/MsgReceiverManager.java`, `/home/shadowbroker/tryx_panorama_linux/aio-reverse/homeui-decompiled/sources/com/baiyi/homeui/tkcfanhomeui/MainActivity.full.java`, `/home/shadowbroker/tryx_panorama_linux/aio-reverse/homeui-decompiled/sources/com/baiyi/homeui/tkcfanhomeui/entity/Warning.java`.
