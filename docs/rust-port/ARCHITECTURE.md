# Архитектура и контракты

Этот документ — контракты, которые реализует каждый пакет порта: структура Cargo workspace, модель исполнения (tokio, акторы), правила для всех крейтов и публичные сигнатуры крейтов. Агенты пакетов реализуют именно эти сигнатуры. Если контракт нужно изменить, в том же коммите правятся ARCHITECTURE.md и спеки затронутых пакетов (`wp/WP-XX.md`), а в «Заметки» PROGRESS.md пишется строка (PROCESS.md, п. 3). В конце — таблица «C++ → Rust»: какой крейт и пакет заменяет каждый файл C++-эталона.

## Workspace

```
satellite/
  Cargo.toml              # [workspace] members = ["crates/*"], resolver = "3", [workspace.dependencies], [profile.release]
  Cargo.lock              # коммитится
  rust-toolchain.toml     # channel = "<текущий stable на момент WP-00, >= 1.88>", targets aarch64-unknown-linux-gnu + armv7-unknown-linux-gnueabihf, components rustfmt + clippy, profile minimal
  Makefile                # обёртка: make (оба arch), make ARCH=x binary|tools, make check (host)
  crates/
    pulsar-core/          # WP-00: логгер, Timers, actors, sysfs, net, tz, Model, общие типы
    pulsar-alsa/          # WP-00: dlopen libasound, bin alsa_abi_probe
    pulsar/               # WP-00 main + crash; WP-12 controller/
    pulsar-esphome/       # WP-01, WP-02 (bin api_selftest)
    pulsar-lan/           # WP-03: mdns, arbiter (bin lan_probe)
    pulsar-audio/         # WP-04 (bins ww_offline, frontend_probe)
    pulsar-media/         # WP-05: player, mixer (bin player_probe)
    pulsar-sendspin/      # WP-06: shim/ (C++), Rust-сервис (bin sendspin_probe)
    pulsar-hw/            # WP-07: leds, led_fx, input (bin hw_probe; WP-08 его дополняет)
    pulsar-clock/         # WP-08: clock, clock_fx, light_sensor
    pulsar-zigbee/        # WP-09, WP-10 (bin zigbee_probe)
    pulsar-update/        # WP-11 (bin update_probe)
  third_party/            # fetch.sh, CMakeLists.txt, toolchain/ (только для sendspin-cpp)
```

- Edition 2024 во всех крейтах. Внешние зависимости берутся только из `[workspace.dependencies]` (`dep = { workspace = true }`). Новую внешнюю зависимость агент добавляет в корневой `Cargo.toml` и обосновывает в отчёте ревью.
- Утилиты лежат в `crates/<крейт>/src/bin/<имя>.rs`. Makefile находит их глобом `crates/*/src/bin/*.rs`, поэтому общих списков нет.
- `[workspace.dependencies]` заводятся в WP-00 с точной фиксацией `version = "=x.y.z"` (берётся последняя версия на момент WP-00 через `cargo search`). Неиспользуемые зависимости в Cargo.lock не попадают. Пакет, который первым начинает использовать зависимость, один раз запускает `cargo check` без `--locked` в `satellite/` и коммитит обновлённый Cargo.lock.
  - `tokio` = `{ default-features = false, features = ["rt", "net", "time", "sync", "macros", "signal", "process", "io-util", "fs"] }`
  - `tokio-rustls` = `{ default-features = false, features = ["ring", "tls12", "logging"] }`
  - `rustls` = `{ default-features = false, features = ["ring", "std", "tls12", "logging"] }`
  - `rustls-pki-types` = `{ features = ["std"] }`
  - `ring`, `log` (`features = ["std"]`), `libc`, `libloading`
  - `snow` (фичи по умолчанию, чистый Rust)
  - `socket2` = `{ features = ["all"] }`
  - `cc` (build-зависимость шима)
  - для dev-зависимостей: `tokio` с фичей `test-util`
- `[profile.release]`: `opt-level = 3`, `lto = "thin"`, `codegen-units = 4`, `panic = "abort"`, `debug = "line-tables-only"`, `strip = false` (strip делает Makefile).

## Модель исполнения (tokio, акторы)

1. В процессе один рантайм `tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(4)` и `LocalSet`. Рантайм уничтожается через `shutdown_timeout(Duration::from_secs(1))`: обычный drop ждёт `spawn_blocking` бесконечно. Все задачи порождаются через `pulsar_core::actors::spawn_actor(name, fut)`: он вызывает `spawn_local` и регистрирует JoinHandle в thread-local реестре (у параллельных потоков `cargo test` реестры не пересекаются). `tokio::spawn` не используется.
2. Каждый сервис устроен одинаково:
   - `pub fn spawn(cfg…) -> (XHandle, UnboundedReceiver<XEvent>)`. Сервисы без событий возвращают только handle.
   - Синхронное открытие ресурсов (bind, open, dlopen) происходит внутри `spawn`. Ошибку `spawn` логирует как C++. Дальше всё зависит от того, что делает с объектом C++-контроллер:
     - он продолжает пользоваться объектом → `spawn` возвращает деградированный handle с тем же поведением, что у C++-объекта после неудачного `open()`/`init()`. Так работают Player, Leds, Clock, AudioFrontend, Mixer;
     - он объект сбрасывает или не подписывается на него → `spawn` возвращает `Result<_, String>`. Так работают ApiServer (там это код выхода 1), Sendspin, Input, LightSensor, mDNS.
     - Конкретные сигнатуры — в контрактах ниже.
   - `XHandle`: `Clone`, все методы принимают `&self`, не `async` и никогда не блокируют. Команды уходят в `tokio::sync::mpsc::UnboundedSender<Cmd>` и обрабатываются актором в порядке FIFO. Геттеры читают разделяемое состояние, которое актор (или рабочий поток) обновляет до отправки соответствующего события.
   - Где живут handle и разделяемое состояние. Все handle живут в `LocalSet` контроллера и могут быть `!Send`. Поля, которые пишут только на потоке рантайма (handle и актор), — `Rc<Cell>`/`Rc<RefCell>`; borrow не держится через `.await` и не уходит в FFI-колбэки. Поля, которые пишет `std::thread`, — `Arc` + атомики; `Relaxed` достаточно, порядок относительно события задаёт канал.
   - Владение полем, которое пишет handle. Такое поле существует в одном экземпляре, в общей ячейке. Актор читает и меняет его на месте, но не держит копию и не публикует как снимок (сейчас это `UpdaterHandle::busy`).
   - Синхронность геттеров. Если C++-метод менял наблюдаемое геттером состояние синхронно (внутри вызова), handle-метод обновляет разделяемое состояние сам, до отправки команды и при тех же условиях, что C++, а актор только продвигает его дальше. В этом случае геттер не возвращает состояние «до команды», которую контроллер уже вызвал. Если C++ меняет состояние только асинхронно (рабочий поток, колбэк библиотеки, сетевое событие), геттер ведёт актор или рабочий поток: синхронное обновление в handle было бы новым поведением. Значения, которые C++ получает в `open()`/`start()`, `spawn` вычисляет синхронно до возврата handle. Разбор по геттерам (подробно — в комментариях контрактов ниже):
     - синхронно в handle-методе: `UpdaterHandle::busy` (CAS в `check`/`install`, актор сбрасывает перед отправкой события);
     - синхронно в `spawn`, дальше актор или константа:
       - `InputHandle::hw_muted` (`EVIOCGSW`);
       - `ArbiterHandle::ok` (итог открытия сокета: socket / `SO_REUSEADDR` / `SO_BROADCAST` / bind, не меняется);
       - `MdnsHandle::ipv4` (выбор интерфейса; дальше netlink, опрос 30 с);
       - `ApiHandle::mdns_txt` (`load_key`; дальше актор пересчитывает его после `set_key` + 100 мс, до `MdnsTxtChanged`);
       - `ZigbeeHandle::mode` (константа);
       - `ZigbeeHandle::status`: начальная фаза считается в `spawn` («off» / «starting» / «error: cannot open the radio», `service.cpp:58-89`), и первый `Changed` уже лежит в канале, когда `spawn` возвращается; дальше статус ведёт актор (после `join`/`leave` — см. «намеренное отличие» ниже);
       - `Mixer` синхронный целиком (актора нет): `open()` берёт raw, только если чтение удалось и raw > 0, иначе остаётся 0.5;
     - асинхронно и в C++ (геттер ведёт актор/рабочий поток, сразу после команды он, как и в C++, отдаёт прежнее значение):
       - `PlayerHandle::state`;
       - `SendspinHandle::streaming` — меняют колбэки потока, обновление группы со `stopped` и разрыв, найденный в `poll()`; удержание 1 с — только для конца без `stopped`;
       - `ZigbeeHandle::flashing`;
     - живое чтение состояния библиотеки: `SendspinHandle::connected` — неблокирующее атомарное чтение (`connection_manager_->is_connected()` = `connected_.load(acquire)`, документировано как вызываемое из любого потока); handle (он и так `!Send` из-за `transport()`) зовёт `is_connected` шима синхронно на потоке рантайма, без флага и без `spawn_blocking`;
     - намеренное отличие от C++:
       - `ApiHandle::{connected, voice_subscribed}`. В C++ ошибка записи или переполнение backlog («client not reading») синхронно закрывали клиента внутри `voice_request_start`/`voice_request_stop`/`voice_send_audio`/`voice_announce_finished`/`publish_*`. В Rust ввод-вывод делает актор, поэтому сразу после вызова геттер ещё отдаёт прежнее значение, а итог приходит только событиями: `VoiceStartFailed{run}`, `VoiceAudioOverflow{run}`, `VoiceSubscribed(false)` и, возможно, `VoiceSubscribed(true)` (передача голоса другому клиенту), `ClientDisconnected`. Контроллер не судит о результате по геттеру сразу после вызова.
       - `ZigbeeHandle::status` после `join`/`leave`. C++ меняет `joining`/`error` синхронно (`router.cpp:50-51, 72-73`); в Rust изменение видно только после того, как актор обработает команду (флагов на стороне handle нет). Это допустимо: WP-12 читает `status()` только по `Changed` (join шлёт его после изменения) и в `on_client_connected`. Окно: если `ClientConnected` обработан между `join()` и актором, публикуется прежний статус, и следующий `Changed` его исправляет.
   - События идут через `tokio::sync::mpsc::UnboundedReceiver<XEvent>`. Контроллер сам владеет своим состоянием и выбирает события через `tokio::select!`. Поэтому реентрантных вызовов, как у C++-хуков, нет.
   - Ответ на запрос, который можно отменить или вытеснить (воспроизведение, арбитраж, голосовой run), несёт id, возвращённый методом handle (`PlayId`, `claim → u64`, `voice_request_start → u64`). Устаревшие id контроллер игнорирует. Этим заменяются C++-колбэки с захватом `seq` и проверки `dialog_` после синхронных вызовов. У проверки и установки обновлений id только связывает ответ с запросом: в C++ они не отменяются, и колбэки не несут seq.
3. Таймеры внутри актора: `pulsar_core::Timers<K>` (ключ — enum актора). Семантика повторяет C++ `Loop` (`core/loop.cpp:84-100`):
   - на каждом пробуждении берётся снимок просроченных ключей; они срабатывают в порядке последнего (пере)взвода, как C++ id;
   - ключ, отменённый более ранним срабатыванием того же снимка, пропускается;
   - повторяющийся таймер перевзводится на `now + period` до возврата ключа;
   - `next()` cancel-safe;
   - срабатывание ключей одного снимка подряд, без других событий между ними, гарантируется, только если `timers.next()` — первая ветка `tokio::select!` с `biased;`.
4. Блокирующие и реалтайм-циклы — отдельные `std::thread` с именем: захват и обработка ALSA, GLib main loop, sink-колбэк sendspin-cpp, XMODEM-прошивка радио (последняя также может идти в `spawn_blocking`). Наружу они отправляют данные через `UnboundedSender` (Send), команды получают через атомики, `Mutex` или `std::sync::mpsc`.
5. Остановка.
   - Актор завершается, когда закрыт его канал команд (все handle уничтожены). При выходе он выполняет teardown C++-деструктора: гасит LED, отпускает reset радио, шлёт mDNS goodbye.
   - Актор без handle (датчик света) завершается, когда отправка события вернула ошибку, то есть получатель уничтожен.
   - `main` после выхода контроллера уничтожает handle и ждёт `actors::join_all(3 s)`. К этому моменту уже взведён watchdog teardown на 5 с.
   - Потоки останавливаются в `Drop` своего владельца (флаг + join).
6. Не держать `RefCell`-borrow через `.await`. Состояние актора хранится по значению в его задаче.
7. Логи пишутся через `log::{debug,info,warn,error}!`, бэкенд — в `pulsar-core`. Тексты сообщений берутся из C++, где они несут смысл для пользователя. Записи чужих крейтов (target не начинается с `pulsar`) идут только в stderr и только с уровня Warn, в ESPHome-sink не попадают. `Trace` отбрасывается.
8. `unsafe` и все вызовы `libc` (включая `std::env::set_var`) живут только в модулях `sys.rs`/`ffi.rs` и в обработчике крашей.
   - В каждом крейте `#![deny(unsafe_op_in_unsafe_fn)]`, у каждого блока комментарий `// SAFETY:`.
   - Символы dlopen резолвятся один раз в структуру указателей на функции.
   - Раскладки C-структур — `#[repr(C)]` с compile-time `assert!(core::mem::offset_of!(…) == …)` под `#[cfg(target_arch)]`.
   - Обработчик крашей не аллоцирует и не использует `eprintln!`/`std::io`: только `libc::write(2, …)`.
9. Комментарии на английском и объясняют факты устройства и ограничения, а не код (конвенция AGENTS.md).
10. `make -C satellite check` должен проходить: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` (host), тот же clippy с `--target aarch64-unknown-linux-gnu --target armv7-unknown-linux-gnueabihf` (цели ставит rustup по `rust-toolchain.toml`, линкер не нужен), `cargo test --workspace --locked` (host x86_64, без sendspin-cpp).
    - Clippy линтует host и обе цели. Архитектурно-зависимый код вместе со всеми его хелперами целиком лежит под `#[cfg(target_arch = …)]`, чтобы ни на одной из них не было мёртвого кода.

## `pulsar-core` (WP-00)

```rust
// log: бэкенд log::Log; stderr-строка как в C++: "HH:MM:SS.mmm L file:line msg\n" (локальное время через libc::localtime_r, L из "DIWE", file = basename)
pub fn init_logger();                                        // уровень из PULSAR_LOG_LEVEL (0=Debug,1=Info(по умолчанию),2=Warn,3=Error)
pub fn stderr_level() -> log::LevelFilter;                   // текущий уровень stderr (для шума alsa-lib: только при Debug, как в C++)
pub struct LogLine { pub level: log::Level, pub text: String } // text = "[W][file.rs:12]: message"
pub fn set_log_sink(min_level: Option<log::Level>, notify: Option<std::sync::Arc<tokio::sync::Notify>>); // None = выкл., очередь и счётчик сбрасываются
pub fn take_log_lines() -> Vec<LogLine>;                    // при переполнении (512) первая строка {Warn, "[W][log.rs]: N log lines dropped"}
// правила C++ log.cpp: лимит 1024 байта, схлопывание повторов (file,line,level,msg) с отчётом "last message repeated N times" не чаще 30 с,
// ранний drop (log::set_max_level динамически = самый подробный из уровня stderr и уровня sink), notify только при переходе очереди
// из пустой в непустую, лог изнутри notify — no-op

pub mod fmt {                                                // совместимость state.conf с C++ (controller.cpp:68-152)
    pub fn g6(v: f64) -> String;                            // как C++ ostream по умолчанию / printf "%g": 6 значащих цифр (через libc::snprintf в sys.rs)
    // числа state.conf (g6 / parse_f32_prefix) форматируются и разбираются в C-локали потока (uselocale) независимо от setlocale где-либо в процессе
    pub fn parse_f32_prefix(s: &str) -> f32;                 // как strtof: числовой префикс, иначе 0
}

pub fn now_ms() -> i64;                                       // CLOCK_MONOTONIC
pub struct Timers<K: Copy + Eq> { /* … */ }
impl<K: Copy + Eq> Timers<K> {
    pub fn new() -> Self;
    pub fn set(&mut self, key: K, delay_ms: u64);            // (пере)взводит key
    pub fn set_repeating(&mut self, key: K, delay_ms: u64, period_ms: u64);
    pub fn cancel(&mut self, key: K);
    pub fn is_set(&self, key: K) -> bool;
    pub async fn next(&mut self) -> K;                      // ждёт ближайший; без таймеров — pending навсегда
}
pub mod actors {
    pub fn spawn_actor(name: &'static str, fut: impl std::future::Future<Output = ()> + 'static);
    pub async fn join_all(timeout: std::time::Duration) -> bool; // true = все завершились
}
pub mod sysfs {                                               // порт hw/sysfs.h
    pub fn read_file(path: &str) -> Option<String>;
    pub fn write_file(path: &str, val: &str) -> bool;
    pub fn write_int_fd(fd: std::os::fd::RawFd, v: i32) -> bool; // pwrite с offset 0
    pub fn trim(s: &str) -> &str;
}
pub mod net {                                                 // выбор интерфейса из esphome/mdns.cpp
    pub struct Iface { pub name: String, pub index: u32, pub addr: std::net::Ipv4Addr }
    pub fn best_interface() -> Option<Iface>;               // wlan0=3, iface маршрута по умолчанию (/proc/net/route)=2, иначе 1; IFF_UP|IFF_MULTICAST, не loopback, не 169.254/16
    pub fn score(name: &str, default_route_iface: Option<&str>) -> u32; // чистая часть
}
pub mod tz { pub fn set_tz(posix_tz: &str); }               // set_var("TZ") + libc::tzset в sys.rs; вызывающий зовёт только при изменении значения (гонка getenv с потоками GLib — как в C++)
pub struct AmbientDimmer { /* порт hw/ambient.h */ }
impl AmbientDimmer { pub fn new() -> Self; pub fn update(&mut self, lux: f32) -> bool; pub fn factor(&self) -> f32; }
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MusicFrame { pub bands: [f32; 8], pub loudness: f32, pub beat: bool }
pub const SENDSPIN_FRESH_MS: i64 = 500;
pub const MUSIC_STALE_MS: i64 = 600;
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum UiState { Idle, Listening, Thinking, Speaking, Error, TimerRing }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum ClockShot { Boot, Pause, Next, Previous }
pub mod model {
    // Model / LedStrip / VolumePoint: поля 1:1 с satellite/src/model.h (имена snake_case те же);
    // строки &'static str, списки &'static [..]; значения 1:1 с model.cpp (midi(), lite2())
    pub fn detect_model() -> Option<&'static Model>;        // /proc/device-tree/compatible
    pub fn model_from_compatible(raw: &[u8]) -> Option<&'static Model>; // NUL-разделённый список; первый yandex,cucumber|yandex,plum
}
```

## `pulsar-alsa` (WP-00)

```rust
// dlopen("libasound.so.2"); для hw_params set_rate_near/set_period_size_near/set_buffer_size_near/get_period_size/get_buffer_size
// сначала dlsym "__snd_pcm_hw_params_*", затем обычное имя (причина — satellite/src/audio/alsa_abi.h)
pub fn load() -> Result<(), String>;                        // идемпотентно; все остальные вызовы сами вызывают load(); первая неудача один раз пишет error! с текстом dlerror, дальше open → -ENOENT
// внутренняя load_from(soname: &str) -> Result<Lib, String>; host-тест вызывает её с несуществующим soname и проверяет Err с текстом dlerror
// (сам load() на host не тестируется: на host-машинах и CI-раннерах libasound обычно установлен)
pub fn has_modern_aliases() -> bool;                        // для alsa_abi_probe ("alias=yes|no")
pub fn strerror(err: i32) -> String;
pub fn quiet_error_handler();                               // snd_lib_error_set_handler: обработчик с невариадической сигнатурой (file,line,function,err,fmt) пишет log::debug без fmt, только при pulsar_core::stderr_level() >= Debug
#[derive(Clone, Copy)] pub enum Stream { Playback, Capture }
#[derive(Clone, Copy, PartialEq, Eq)] pub enum Format { S16Le, S24_3Le, S32Le }
pub struct HwRequest { pub format: Format, pub channels: u32, pub rate: u32, pub rate_exact: bool, pub period_frames: Option<u64>, pub buffer_frames: Option<u64> }
pub struct HwResult { pub rate: u32, pub period_frames: u64, pub buffer_frames: u64 }
#[derive(Clone, Copy, Debug)]
pub struct HwTrace { pub near: HwResult /* запрос после set_*_near */, pub actual: HwResult /* прочитано после apply, иначе 0 */,
                     pub failed: Option<(&'static str, i32)> /* "any","access","format","channels","rate","period","buffer","apply","get period","get buffer" */ }
pub struct Pcm { /* snd_pcm_t*, Send */ }
impl Pcm {
    pub fn open(name: &str, stream: Stream, nonblock: bool) -> Result<Pcm, i32>;   // Err = отрицательный errno ALSA
    pub fn set_hw_params(&mut self, req: &HwRequest) -> Result<HwResult, i32>;   // access RW_INTERLEAVED
    pub fn set_hw_params_traced(&mut self, req: &HwRequest) -> HwTrace;           // для alsa_abi_probe: значения каждого шага и имя упавшего
    pub fn set_sw_params(&mut self, start_threshold: u64, avail_min: u64) -> Result<(), i32>;
    pub fn prepare(&mut self) -> Result<(), i32>;
    pub fn drop_frames(&mut self) -> Result<(), i32>;      // snd_pcm_drop
    pub fn resume(&mut self) -> Result<(), i32>;
    pub fn readi(&mut self, buf: &mut [i16], frames: usize) -> Result<usize, i32>;
    pub fn writei(&mut self, buf: &[u8], frames: usize) -> Result<usize, i32>;
    pub fn wait(&mut self, timeout_ms: i32) -> Result<bool, i32>;
    pub fn avail_update(&mut self) -> Result<usize, i32>;
    pub fn delay(&mut self) -> Result<i64, i32>;
    pub fn recover(&mut self, err: i32, silent: bool) -> Result<(), i32>;
}                                                           // Drop = snd_pcm_close
pub struct Ctl { /* snd_ctl_t* */ }
impl Ctl {
    pub fn open(name: &str) -> Result<Ctl, i32>;           // "hw:0"
    pub fn mixer_exists(&self, name: &str) -> bool;         // SND_CTL_ELEM_IFACE_MIXER, через *_malloc/_free вместо alloca
    pub fn read_integer(&self, name: &str, index: u32) -> Result<i64, i32>;
    pub fn write_integers(&self, name: &str, values: &[i64]) -> Result<(), i32>;
}
```

## `pulsar-esphome` (WP-01 + WP-02)

```rust
pub struct DeviceIdentity { pub name: String, pub friendly_name: String, pub mac: String /* "AA:BB:.." */, pub model: String,
    pub board: String, pub manufacturer: String, pub project_version: String, pub compilation_time: String }
pub struct WakeWordInfo { pub id: String, pub phrase: String, pub languages: Vec<String> }
pub struct ApiConfig { pub port: u16 /* 6053 */, pub wake_words: Vec<WakeWordInfo>, pub active_wake_words: Vec<String>,
    pub light_effects: Vec<String>, pub clock_faces: Vec<String> /* "None" первым */, pub music_styles: Vec<String>,
    pub case_colors: Vec<String> /* "None" первым */, pub clock_music_styles: Vec<String>,
    pub zigbee_modes: Vec<String> /* пусто = без Zigbee-сущностей */, pub key_path: Option<std::path::PathBuf> }
// enum'ы и структуры 1:1 с satellite/src/esphome/api_server.h (те же числовые значения, #[repr(u32)] где значение уходит в протокол):
// VaEvent, TimerEvent, TimerInfo, AnnounceRequest, MediaState, MediaCommand, LightState, SwitchId, SensorId, BinarySensorId,
// NumberId, TextId, SelectId, ButtonId, TextSensorId, ActionId, EventId, UpdateCommand, UpdateState
pub struct MediaCommandRequest { pub command: Option<MediaCommand>, pub volume: Option<f32>, pub media_url: Option<String>, pub announcement: Option<bool> }
pub enum ApiEvent {
    ClientConnected { client_info: String }, ClientDisconnected, VoiceSubscribed(bool),
    VoiceEvent { event: VaEvent, data: std::collections::HashMap<String, String> }, Announce(AnnounceRequest), Timer(TimerInfo),
    ActiveWakeWords(Vec<String>), MediaCommand(MediaCommandRequest), Switch(SwitchId, bool), Button(ButtonId),
    Light { state: LightState, transition_ms: u32 }, Number(NumberId, f32), Text(TextId, String), Select(SelectId, String),
    Action { id: ActionId, value: String, seconds: i32 }, LedShow { effect: String, color: String, seconds: i32 },
    Time { epoch: u32, posix_tz: String }, Restart, VoiceStartFailed { run: u64 }, VoiceAudioOverflow { run: u64 }, Update(UpdateCommand),
    MdnsTxtChanged(Vec<String>),                             // после смены ключа: новый TXT для _esphomelib._tcp
}
pub fn spawn(id: DeviceIdentity, cfg: ApiConfig) -> Result<(ApiHandle, tokio::sync::mpsc::UnboundedReceiver<ApiEvent>), String>;
impl ApiHandle {
    pub fn connected(&self) -> bool;  pub fn voice_subscribed(&self) -> bool;
// connected()/voice_subscribed(): намеренное отличие от C++ (см. «Модель исполнения» п.2). В C++ ошибка записи или переполнение backlog
// («client not reading», api_server.cpp flush/close_client) закрывали клиента синхронно внутри voice_request_start/voice_request_stop/
// voice_send_audio/voice_announce_finished/publish_*. В Rust ввод-вывод делает актор: сразу после вызова геттер отдаёт прежнее значение, итог
// приходит событиями VoiceStartFailed{run}, VoiceAudioOverflow{run}, VoiceSubscribed(false) и, возможно, VoiceSubscribed(true) (close_client →
// update_voice_owner передаёт голос другому клиенту), ClientDisconnected; геттеры актор обновляет до отправки события.
// mdns_txt(): считается в spawn (load_key), после set_key + 100 мс актор пересчитывает его до MdnsTxtChanged.
    pub fn mdns_txt(&self) -> Vec<String>;                  // TXT _esphomelib._tcp для текущего состояния ключа
    pub fn voice_request_start(&self, wake_word_phrase: &str) -> u64 /* run id, растёт с 1 */;  pub fn voice_request_stop(&self);
    pub fn voice_send_audio(&self, pcm: Vec<i16>);           pub fn voice_announce_finished(&self, success: bool);
    pub fn set_active_wake_words(&self, ids: Vec<String>);
    // WP-02:
    pub fn publish_media(&self, st: MediaState, volume: f32, muted: bool);
    pub fn publish_switch(&self, id: SwitchId, on: bool);   pub fn publish_binary_sensor(&self, id: BinarySensorId, on: bool);
    pub fn publish_sensor(&self, id: SensorId, v: f32);      pub fn publish_number(&self, id: NumberId, v: f32);
    pub fn publish_text(&self, id: TextId, s: &str);         pub fn publish_select(&self, id: SelectId, s: &str);
    pub fn publish_text_sensor(&self, id: TextSensorId, s: &str); pub fn publish_light(&self, st: &LightState);
    pub fn fire_event(&self, id: EventId, event_type: &str); pub fn publish_update(&self, st: &UpdateState);
}
```
mDNS вынесен из ApiServer. Респондер создаёт контроллер, ApiServer только отдаёт `mdns_txt()` и событие `MdnsTxtChanged`. IP для сенсора «IP address» берётся из `pulsar_core::net::best_interface()`.

## `pulsar-lan` (WP-03)

```rust
pub mod mdns {
    pub struct MdnsService { pub service: String, pub instance: String, pub port: u16, pub txt: Vec<String> }
    pub fn spawn(host: String) -> Result<MdnsHandle, String>;  // bind 5353 не удался -> эфемерный порт, только анонсы (как C++)
    impl MdnsHandle { pub fn add_service(&self, s: MdnsService); pub fn set_txt(&self, service: &str, txt: Vec<String>);
                      pub fn ipv4(&self) -> Option<std::net::Ipv4Addr>; }
                      // ipv4(): выбор интерфейса синхронно в spawn (как refresh_iface() в C++ start()), дальше обновляет актор (netlink, опрос 30 с);
                      // add_service/set_txt его не меняют
}
pub mod arbiter {
    pub struct Claim { pub station: String, pub wake: String, pub busy: bool, pub level_db: f32 }
    pub enum ArbiterEvent { Decided { claim_id: u64, won: bool, best: Claim, peers: usize } }
    pub fn spawn(station: String, port: u16) -> (ArbiterHandle, tokio::sync::mpsc::UnboundedReceiver<ArbiterEvent>); // сокет не открылся -> warn, ok()=false, claim(decide=true) сразу даёт Decided{won:true, peers:0}
    impl ArbiterHandle { pub fn ok(&self) -> bool;
                         // ok(): итог открытия сокета в spawn (socket / SO_REUSEADDR / SO_BROADCAST / bind), не меняется
        pub fn claim(&self, wake: &str, level_db: f32, busy: bool, decide: bool) -> u64; // decide=false = только рассылка (busy-claim при звонке таймера)
        pub fn cancel(&self); }
}
```

## `pulsar-audio` (WP-04)

```rust
pub enum FrontendEvent { Wake { phrase: String, level_db: f32 }, Command { word: String }, Audio(Vec<i16>) /* 16 кГц моно, 512 */, Music(pulsar_core::MusicFrame) }
pub fn spawn(model: &'static pulsar_core::model::Model, wake_model_dir: std::path::PathBuf, command_model_dir: Option<std::path::PathBuf>)
    -> (FrontendHandle, tokio::sync::mpsc::UnboundedReceiver<FrontendEvent>); // ошибка dlopen/VQE/спотера логируется, handle деградированный (событий нет), как C++ после неудачного start()
impl FrontendHandle { pub fn set_model(&self, dir: std::path::PathBuf); pub fn set_detection(&self, on: bool); pub fn set_commands(&self, on: bool);
                      pub fn set_streaming(&self, on: bool); pub fn set_music_analysis(&self, on: bool); }
pub struct SpectrumAnalyzer;                                 // pub: new(rate, chunk_frames), ok(), reset(), process(&[f32], nspk, &mut MusicFrame) -> bool
```

## `pulsar-media` (WP-05)

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Channel { Music, Announce }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum PlayState { Idle, Loading, Playing, Paused, Error }
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)] pub struct PlayId(pub u64);
pub enum PlayerEvent { State(Channel, PlayState), Done { id: PlayId, ok: bool } } // Done ровно один раз на каждый play(); вытеснение/stop -> ok=false
pub fn spawn() -> (PlayerHandle, tokio::sync::mpsc::UnboundedReceiver<PlayerEvent>);
// ошибка dlopen/gst_init логируется ("player init failed"), handle деградированный: каждый play() -> Done{id, ok:false}, state() = Idle (как player.cpp:612-615)
impl PlayerHandle { pub fn play(&self, ch: Channel, urls: Vec<String>) -> PlayId; pub fn pause(&self, ch: Channel); pub fn resume(&self, ch: Channel);
                    pub fn stop(&self, ch: Channel); pub fn state(&self, ch: Channel) -> PlayState; pub fn set_channel_gain(&self, ch: Channel, gain: f32); }
// state(ch) пишет только рабочий поток GLib (атомик), как в C++: команды уходят туда через g_idle_add, поэтому play/pause/resume/stop
// синхронно state не меняют и сразу после команды геттер отдаёт прежнее состояние (как C++). Атомик обновляется до отправки State.
pub struct Mixer;                                            // синхронный, владеет контроллер
impl Mixer { pub fn open(model: &'static pulsar_core::model::Model) -> Mixer; // без контрола: available()=false, volume() хранится в памяти (по умолчанию 0.5), как C++
             pub fn available(&self) -> bool; pub fn volume(&self) -> f32; pub fn set_volume(&mut self, v: f32);
             pub fn muted(&self) -> bool; pub fn set_muted(&mut self, m: bool); }
// Mixer синхронный: open() берёт raw, только если чтение удалось и raw > 0 (иначе 0.5); set_volume/set_muted сразу пишут контрол и поля,
// геттеры читают поля
pub mod mixer {                                               // чистые кривые (player.cpp ~700-745), работают и без ALSA
    pub fn to_raw(model: &Model, v: f32) -> i64; pub fn from_raw(model: &Model, raw: i64) -> f32;
    pub fn to_legacy(model: &Model, v: f32) -> f32; pub fn from_legacy(model: &Model, v: f32) -> f32;
}
// WP-05 также вызывает pulsar_alsa::quiet_error_handler() при старте плеера (как Player::init, player.cpp:559-560)
```

## `pulsar-sendspin` (WP-06)

```rust
pub struct SendspinConfig { pub name: String, pub product_name: String, pub manufacturer: String, pub software_version: String,
                            pub mac: String, pub state_dir: std::path::PathBuf, pub port: u16 }
#[derive(Clone, Copy)] pub enum Transport { Play, Pause, Stop, Next, Previous }
pub enum SendspinEvent { Stream(bool), Volume(f32), Mute(bool), PairingCode(String) /* "" = убрать */, PairingWindow(bool), Music(pulsar_core::MusicFrame) }
pub fn spawn(cfg: SendspinConfig, volume: f32, muted: bool) -> Result<(SendspinHandle, tokio::sync::mpsc::UnboundedReceiver<SendspinEvent>), String>;
// без sendspin-cpp (host-сборка, cfg sendspin_ffi не задан): Err("sendspin: built without sendspin-cpp")
impl SendspinHandle { pub fn streaming(&self) -> bool; pub fn connected(&self) -> bool; pub fn set_duck_gain(&self, g: f32);
    pub fn report_volume(&self, volume: f32, muted: bool); pub fn transport(&self, t: Transport) -> bool; // синхронно
    pub fn yield_to_local(&self); pub fn confirm_pairing_window(&self); }
// streaming(): меняют колбэки начала/конца потока, обновление группы со stopped (завершает удерживаемый конец) и разрыв, найденный в poll();
// удержание 1 с — только для конца без stopped. yield_to_local() лишь шлёт client/leave и streaming синхронно не меняет (как C++).
// connected(): неблокирующее атомарное чтение (connection_manager_->is_connected() = connected_.load(acquire), «any thread»): handle
// синхронно на потоке рантайма вызывает is_connected шима, без разделяемого флага и без spawn_blocking.
```

### Рецепт линковки C++ (доказан в WP-00)

Временный крейт `cxx-link-probe` (C++20, `std::string`, `throw`/`catch` в `extern "C"`-функции) собран `cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.36` и `--target armv7-unknown-linux-gnueabihf.2.30` в окружении цели `binary` (`RUSTFLAGS='-C relocation-model=static -C force-frame-pointers=yes'`, zig 0.14.1 в `PATH`, без `CC_*`/`CXX_*`), cargo-zigbuild 0.24.0, `cc` 1.10.0. Достаточно минимального рецепта:

```rust
// build.rs
cc::Build::new().cpp(true).std("c++20").file("…").compile("…");
println!("cargo:rustc-link-lib=c++");
```

- `c++abi` и `-Wl,--start-group`/`--end-group` не понадобились: zig сам линкует свои libc++, libc++abi и libunwind статически. Для статических библиотек sendspin-cpp с перекрёстными ссылками `--start-group` остаётся запасным путём (см. «Допущения»).
- С WP-07 для armhf в `RUSTFLAGS` добавлен `-C target-cpu=cortex-a53`, как `-mcpu=cortex_a53` у C++-сборки: `mul_add` становится аппаратным `vfma` вместо вызова `fmaf` (паритет FMA с C++, см. `pulsar-hw` `led_fx`).
- non-PIE линкуется: оба бинаря `EXEC`. `NEEDED`: aarch64 — только `libc.so.6` (максимум `GLIBC_2.34`); armhf — `libpthread.so.0`, `libc.so.6`, `libdl.so.2`, `ld-linux-armhf.so.3` (максимум `GLIBC_2.30`), `hard-float ABI`.
- При компиляции `cc` печатает безвредные предупреждения проверки компилятора через обёртки cargo-zigbuild: aarch64 — `zig: error: version '.2.36' in target triple 'aarch64-unknown-linux-gnu.2.36' is invalid`, armhf — `…/cargo-zigbuild/0.24.0/aeabi_unaligned.c:1:1: error: FileNotFound`. Объект при этом собирается и линкуется (`cxx_probe`, `__cxa_throw`, `_Unwind_RaiseException` в бинаре).
- Запуск на станциях: на Midi (aarch64) и Lite 2 (armhf) бинарь печатает `cxx-link-probe: caught exception ok`, rc=0 (WP-00, `reviews/WP-00.md`).

## `pulsar-hw` (WP-07) и `pulsar-clock` (WP-08)

```rust
// pulsar-hw
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct Rgb { pub r: u8, pub g: u8, pub b: u8 }
pub fn spawn_leds(model: &'static Model) -> LedsHandle;    // ошибка открытия fb/sysfs логируется ("LEDs unavailable"), вывод no-op, состояние и set_case_color работают
impl LedsHandle { set_ui_state(UiState); set_muted(bool); set_user_light(on: bool, color: Rgb, brightness: f32, effect: String, transition_ms: u32);
    show_volume(f32); set_ambient_lux(f32); show_timer_started(); show_temporary(effect: String, color: Rgb, seconds: i32); clear_temporary();
    set_voice_indication(bool); set_speed(f32); set_music_style(String); set_media_playing(bool); music_frame(MusicFrame, from_sendspin: bool);
    set_case_color(&self, stem: &str) -> bool /* синхронно грузит матрицу из /system/vendor/quasar/ledconfigs/<stem>.json, работает и без открытых LED (leds.cpp:618-634); false = нет файла/ошибка разбора */ }  // все методы &self
pub fn effect_names() -> &'static [&'static str]; pub fn music_styles() -> &'static [&'static str]; pub fn case_colors() -> Vec<String>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Button { Action, VolumeUp, VolumeDown }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Press { Short, Long }
pub enum InputEvent { Button(Button, Press), Mute(bool) }
pub fn spawn_input(model: &'static Model) -> Result<(InputHandle, UnboundedReceiver<InputEvent>), String>; // Err, если не открылось ни одно устройство
impl InputHandle { pub fn hw_muted(&self) -> bool; }
// hw_muted(): начальное значение (EVIOCGSW) читается синхронно в spawn_input без события Mute (в C++ open() идёт до on_mute); дальше — актор
// pulsar-clock
pub fn spawn(model: &'static Model) -> ClockHandle;        // ошибка открытия логируется ("clock unavailable"), вывод no-op
impl ClockHandle { set_enabled(bool); set_brightness(f32); set_ambient_lux(f32); set_timer(ms_left: i64, running: bool); flash_number(i32);
    show_code(String); clear_code(); set_text(String); set_face(i32); set_scroll_ms(i32); show_temporary_text(String, i32);
    show_temporary_face(i32, i32); set_ui_state(UiState); set_voice_indication(bool); play(ClockShot); set_updating(bool);
    set_music_style(String); set_media_playing(bool); music_frame(MusicFrame, from_sendspin: bool) }
pub fn face_names() -> &'static [&'static str]; pub fn music_styles() -> &'static [&'static str];
pub fn spawn_light_sensor(period_ms: u64) -> Result<UnboundedReceiver<f32>, String>;  // первое значение сразу
```

## `pulsar-zigbee` (WP-09 + WP-10)

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Mode { Off, Coordinator, Router }
pub struct Role { pub mode: Mode, pub name: &'static str, pub label: &'static str, pub family: u8 }
pub const ROLES: [Role; 3];   // {Off,"off","Off",0}, {Coordinator,"pulsar-coordinator","Coordinator",0x51}, {Router,"router","Router",0x50}
pub fn role(m: Mode) -> &'static Role;  pub fn parse_mode(s: &str) -> Mode; // всё неизвестное, включая "coordinator", -> Off
pub struct RadioLines { pub tty: String, pub reset: String, pub boot: String }
pub struct ZigbeeConfig { pub lines: RadioLines, pub coordinator_image: PathBuf, pub router_image: PathBuf, pub deconz_port: u16, pub failed_version: u32 }
pub enum ZigbeeEvent { Changed, FlashOutcome { failed_version: u32 } }   // C++ changed() и outcome(failed)
pub fn spawn(cfg: ZigbeeConfig, mode: Mode) -> (ZigbeeHandle, UnboundedReceiver<ZigbeeEvent>);
impl ZigbeeHandle { pub fn mode(&self) -> Mode; pub fn flashing(&self) -> bool; pub fn status(&self) -> String; pub fn join(&self); pub fn leave(&self); }
// mode(): константа из spawn (смена роли = рестарт демона). flashing(): ведёт актор — запись начинается по ответу радио или таймауту, не по команде.
// status(): начальная фаза считается в spawn («off» / «starting» / «error: cannot open the radio», service.cpp:58-89), первый Changed уже в канале,
// когда spawn возвращается; дальше статус ведёт актор. join(): актор проверяет условия C++ (фаза Running и роутер, затем ready && !joined &&
// !joining), ставит joining = true и очищает error, шлёт kSteer, взводит таймер 90 с и отправляет Changed (C++ router.cpp:66). leave(): актор
// при тех же условиях (Running, роутер, ready) ставит joining = false и очищает error, без Changed (как C++).
```
Внутренний API между WP-09 и WP-10 (link, coordinator, router, flasher) задаёт WP-09 по тем же правилам и документирует в rustdoc крейта.

## `pulsar-update` (WP-11)

```rust
#[derive(Clone, Debug, PartialEq)] pub struct Release { pub version: String, pub url: String, pub bin_url: String, pub sums_url: String }
pub fn release_newer(latest: &str, current: &str) -> (bool /*newer*/, bool /*current_known*/);
pub enum UpdateEvent { Checked { id: u64, result: Result<Release, String> }, Progress { id: u64, value: f32 /* те же единицы, что у C++ progress */ },
                       Installed { id: u64, result: Result<(), (String, bool /*fatal*/)> } }
pub fn spawn(repo: String, current_version: String) -> (UpdaterHandle, UnboundedReceiver<UpdateEvent>);
impl UpdaterHandle { pub fn busy(&self) -> bool; pub fn check(&self) -> Option<u64>; pub fn install(&self, r: Release, binary: PathBuf) -> Option<u64>;
                     pub fn add_extra_asset(&self, name: &str, path: PathBuf); }
// check()/install(): None = busy, запрос не принят, события не будет (C++ молча возвращался). Some(id) — id из одного общего счётчика с 1.
// busy ставится CAS в handle до отправки команды; актор сбрасывает его непосредственно перед отправкой Checked/Installed. Окно: busy() уже
// false, пока событие ещё в очереди. Для Installed его закрывает update_installing_; для Checked — контроллер, который не начинает установку,
// пока id проверки не обработан (WP-12). Id только связывает ответ с запросом (в C++ не отменяется).
pub fn arch() -> &'static str;   // "aarch64" | "armhf" | "host"
```

## Порты и тестовый режим (реализует WP-12, учитывают WP-03, WP-06, WP-10)

- Базовые порты: ESPHome 6053, Sendspin 8928, deCONZ 6638, арбитраж 6054.
- `PULSAR_PORT_OFFSET=<u16>` (по умолчанию 0) прибавляется ко всем четырём портам через checked add. Если значение не разбирается как u16 или сумма переполняется — лог `invalid PULSAR_PORT_OFFSET` и выход с кодом 1 до запуска сервисов.
- При ненулевом offset контроллер не создаёт mDNS-респондер: HA и Music Assistant тестовый экземпляр не видят.
- Демон всегда берёт порты из конфигов, хардкода нет.

## C++ → Rust

Каждый файл `satellite/src` и `satellite/tools` (эталон — тег `rust-port-base`) отображается в крейт и пакет, который его заменяет и удаляет. Новая утилита без C++-прототипа: `lan_probe` (WP-03, `pulsar-lan`).

| Файл C++ | Крейт | Пакет | Примечание |
|---|---|---|---|
| `satellite/src/arbiter.cpp` | pulsar-lan | WP-03 |  |
| `satellite/src/arbiter.h` | pulsar-lan | WP-03 |  |
| `satellite/src/audio/alsa_abi.h` | pulsar-alsa | WP-00 |  |
| `satellite/src/audio/frontend.cpp` | pulsar-audio | WP-04 |  |
| `satellite/src/audio/frontend.h` | pulsar-audio | WP-04 |  |
| `satellite/src/audio/music_frame.h` | pulsar-core | WP-00 |  |
| `satellite/src/audio/spectrum.cpp` | pulsar-audio | WP-04 |  |
| `satellite/src/audio/spectrum.h` | pulsar-audio | WP-04 |  |
| `satellite/src/controller.cpp` | pulsar (`controller/`, `settings.rs`) | WP-12 |  |
| `satellite/src/controller.h` | pulsar (`controller/`, `settings.rs`) | WP-12 |  |
| `satellite/src/core/log.cpp` | pulsar-core | WP-00 |  |
| `satellite/src/core/log.h` | pulsar-core | WP-00 |  |
| `satellite/src/core/loop.cpp` | pulsar-core | WP-00 |  |
| `satellite/src/core/loop.h` | pulsar-core | WP-00 |  |
| `satellite/src/esphome/api_server.cpp` | pulsar-esphome | WP-02 | транспорт — WP-01, сообщения — WP-02 |
| `satellite/src/esphome/api_server.h` | pulsar-esphome | WP-02 | типы заводит WP-01; файл удаляет WP-02 |
| `satellite/src/esphome/mdns.cpp` | pulsar-lan | WP-03 |  |
| `satellite/src/esphome/mdns.h` | pulsar-lan | WP-03 |  |
| `satellite/src/esphome/proto.h` | pulsar-esphome | WP-01 |  |
| `satellite/src/hw/ambient.h` | pulsar-core | WP-00 |  |
| `satellite/src/hw/clock.cpp` | pulsar-clock | WP-08 |  |
| `satellite/src/hw/clock_fx.cpp` | pulsar-clock | WP-08 |  |
| `satellite/src/hw/clock_fx.h` | pulsar-clock | WP-08 |  |
| `satellite/src/hw/hw.h` | pulsar-hw (`Leds`, `Input`), pulsar-clock (`Clock`, `LightSensor`), pulsar-core (`UiState`, `ClockShot`) | WP-08 | файл удаляет WP-08; `UiState`/`ClockShot` уже в WP-00 |
| `satellite/src/hw/input.cpp` | pulsar-hw | WP-07 |  |
| `satellite/src/hw/led_fx.cpp` | pulsar-hw | WP-07 |  |
| `satellite/src/hw/led_fx.h` | pulsar-hw | WP-07 |  |
| `satellite/src/hw/leds.cpp` | pulsar-hw | WP-07 |  |
| `satellite/src/hw/light_sensor.cpp` | pulsar-clock | WP-08 |  |
| `satellite/src/hw/sysfs.h` | pulsar-core | WP-00 |  |
| `satellite/src/main.cpp` | pulsar (`main.rs`, `crash.rs`, `version.rs`) | WP-00 |  |
| `satellite/src/media/player.cpp` | pulsar-media | WP-05 |  |
| `satellite/src/media/player.h` | pulsar-media | WP-05 |  |
| `satellite/src/model.cpp` | pulsar-core | WP-00 |  |
| `satellite/src/model.h` | pulsar-core | WP-00 |  |
| `satellite/src/sendspin/sendspin_service.cpp` | pulsar-sendspin | WP-06 |  |
| `satellite/src/sendspin/sendspin_service.h` | pulsar-sendspin | WP-06 |  |
| `satellite/src/update/https.cpp` | pulsar-update | WP-11 |  |
| `satellite/src/update/https.h` | pulsar-update | WP-11 |  |
| `satellite/src/update/updater.cpp` | pulsar-update | WP-11 |  |
| `satellite/src/update/updater.h` | pulsar-update | WP-11 |  |
| `satellite/src/zigbee/bytes.h` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/coordinator.cpp` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/coordinator.h` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/deconz_server.cpp` | pulsar-zigbee | WP-10 |  |
| `satellite/src/zigbee/deconz_server.h` | pulsar-zigbee | WP-10 |  |
| `satellite/src/zigbee/flasher.cpp` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/flasher.h` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/router.cpp` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/router.h` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/service.cpp` | pulsar-zigbee | WP-10 |  |
| `satellite/src/zigbee/service.h` | pulsar-zigbee | WP-10 |  |
| `satellite/src/zigbee/telink_link.cpp` | pulsar-zigbee | WP-09 |  |
| `satellite/src/zigbee/telink_link.h` | pulsar-zigbee | WP-09 |  |
| `satellite/tools/alsa_abi_probe.cpp` | pulsar-alsa (`src/bin/alsa_abi_probe.rs`) | WP-00 |  |
| `satellite/tools/api_selftest.cpp` | pulsar-esphome (`src/bin/api_selftest.rs`) | WP-02 |  |
| `satellite/tools/hw_probe.cpp` | pulsar-hw (`src/bin/hw_probe.rs`) | WP-08 | LED/ввод — WP-07, остальное — WP-08; файл удаляет WP-08 |
| `satellite/tools/player_probe.cpp` | pulsar-media (`src/bin/player_probe.rs`) | WP-05 |  |
| `satellite/tools/update_probe.cpp` | pulsar-update (`src/bin/update_probe.rs`) | WP-11 |  |
| `satellite/tools/ww_offline.cpp` | pulsar-audio (`src/bin/ww_offline.rs`) | WP-04 |  |
| `satellite/tools/zigbee_probe.cpp` | pulsar-zigbee (`src/bin/zigbee_probe.rs`) | WP-10 | режимы `info`/`flash` — WP-09; файл удаляет WP-10 |
