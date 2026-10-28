# Архитектура демона

Как устроен код в `satellite/`: workspace, модель исполнения (tokio, акторы, потоки), правила для
всех крейтов, плавающая точка, линковка sendspin-cpp и заметки о поведении сервисов. Сигнатуры
описаны в rustdoc крейтов; здесь — то, что из одного файла не видно.

## Workspace

```
satellite/
  Cargo.toml              # [workspace] members = ["crates/*"], resolver = "3", [workspace.dependencies], [profile.release]
  Cargo.lock              # коммитится
  rust-toolchain.toml     # stable, цели aarch64-unknown-linux-gnu + armv7-unknown-linux-gnueabihf, rustfmt + clippy
  Makefile                # make (обе arch), make ARCH=x [tools], make check (host)
  crates/
    pulsar-core/          # логгер, Timers, actors, sysfs, net, tz, Model, общие типы
    pulsar-alsa/          # dlopen libasound (bin alsa_abi_probe)
    pulsar/               # main, crash, controller/, settings
    pulsar-esphome/       # ESPHome API (bin api_selftest)
    pulsar-lan/           # mdns, arbiter (bin lan_probe)
    pulsar-audio/         # микрофонный фронтенд, спектр (bins ww_offline, frontend_probe)
    pulsar-media/         # player, mixer (bin player_probe)
    pulsar-sendspin/      # shim/ (C++), Rust-сервис (bin sendspin_probe)
    pulsar-hw/            # leds, led_fx, input (bin hw_probe)
    pulsar-clock/         # clock, clock_fx, light_sensor
    pulsar-zigbee/        # link, coordinator, deconz, router, flasher, service (bin zigbee_probe)
    pulsar-update/        # https, updater (bin update_probe)
  third_party/            # fetch.sh, CMakeLists.txt, toolchain/ (только для sendspin-cpp)
```

- Edition 2024 во всех крейтах. Внешние зависимости берутся только из `[workspace.dependencies]`
  (`dep = { workspace = true }`) с точной версией `version = "=x.y.z"`. Новая внешняя зависимость
  добавляется в корневой `Cargo.toml` и требует обоснования.
  - `tokio` = `{ default-features = false, features = ["rt", "net", "time", "sync", "macros", "signal", "process", "io-util", "fs"] }`
  - `tokio-rustls` = `{ default-features = false, features = ["ring", "tls12", "logging"] }`
  - `rustls` = `{ default-features = false, features = ["ring", "std", "tls12", "logging"] }`
  - `rustls-pki-types` = `{ features = ["std"] }`
  - `ring`, `log` (`features = ["std"]`), `libc`, `libloading`
  - `snow` = `{ default-features = false, features = ["use-chacha20poly1305", "use-sha2", "use-curve25519", "use-getrandom"] }` — чистый Rust, только примитивы `Noise_NNpsk0_25519_ChaChaPoly_SHA256`. Фичи по умолчанию не нужны: `std` в snow 0.10.0 включает `ring` (не-weak `ring/std`) вместе с неиспользуемыми AES-GCM и BLAKE2
  - `socket2` = `{ features = ["all"] }`
  - `cc` (build-зависимость шима)
  - для dev-зависимостей: `tokio` с фичей `test-util`
- Утилиты лежат в `crates/<крейт>/src/bin/<имя>.rs`. Makefile находит их глобом
  `crates/*/src/bin/*.rs`, общих списков нет.
- `[profile.release]`: `opt-level = 3`, `lto = "thin"`, `codegen-units = 4`, `panic = "abort"`,
  `debug = "line-tables-only"`, `strip = false` (strip делает Makefile).

## Модель исполнения (tokio, акторы)

1. В процессе один рантайм `tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(4)`
   и `LocalSet`. Рантайм уничтожается через `shutdown_timeout(Duration::from_secs(1))`: обычный
   drop ждёт `spawn_blocking` бесконечно. Все задачи порождаются через
   `pulsar_core::actors::spawn_actor(name, fut)`: он вызывает `spawn_local` и регистрирует
   JoinHandle в thread-local реестре (у параллельных потоков `cargo test` реестры не
   пересекаются). `tokio::spawn` не используется.
2. Каждый сервис устроен одинаково:
   - `pub fn spawn(cfg…) -> (XHandle, UnboundedReceiver<XEvent>)`. Сервисы без событий возвращают
     только handle.
   - Синхронное открытие ресурсов (bind, open, dlopen) происходит внутри `spawn`. Что бывает при
     ошибке, зависит от того, нужен ли объект контроллеру дальше:
     - контроллер продолжает им пользоваться → `spawn` логирует ошибку и возвращает
       деградированный handle (команды принимаются, вывода нет). Так работают Player, Leds, Clock,
       фронтенд микрофона, Mixer;
     - контроллер без него обходится → `spawn` возвращает `Result<_, String>`. Так работают
       ApiServer (там это код выхода 1), Sendspin, Input, LightSensor, mDNS.
   - `XHandle`: `Clone`, все методы принимают `&self`, не `async` и никогда не блокируют. Команды
     уходят в `tokio::sync::mpsc::UnboundedSender<Cmd>` и обрабатываются актором в порядке FIFO.
     Геттеры читают разделяемое состояние, которое актор (или рабочий поток) обновляет до отправки
     соответствующего события.
   - Где живут handle и разделяемое состояние. Все handle живут в `LocalSet` контроллера и могут
     быть `!Send`. Поля, которые пишут только на потоке рантайма (handle и актор), —
     `Rc<Cell>`/`Rc<RefCell>`; borrow не держится через `.await` и не уходит в FFI-колбэки. Поля,
     которые пишет `std::thread`, — `Arc` + атомики; `Relaxed` достаточно, порядок относительно
     события задаёт канал.
   - Поле, которое пишет handle, существует в одном экземпляре, в общей ячейке. Актор читает и
     меняет его на месте, но не держит копию и не публикует как снимок (сейчас это
     `UpdaterHandle::busy`).
   - Синхронность геттеров. Сразу после команды геттер, как правило, отдаёт прежнее значение:
     состояние меняет актор или рабочий поток. Исключения перечислены здесь:
     - синхронно в handle-методе: `UpdaterHandle::busy` (CAS в `check`/`install`, актор
       сбрасывает его перед отправкой события);
     - синхронно в `spawn`, дальше актор или константа:
       - `InputHandle::hw_muted` (`EVIOCGSW`, без события `Mute`);
       - `ArbiterHandle::ok` (итог открытия сокета: socket / `SO_REUSEADDR` / `SO_BROADCAST` /
         bind, не меняется);
       - `MdnsHandle::ipv4` (выбор интерфейса; дальше netlink, опрос 30 с);
       - `ApiHandle::mdns_txt` (`load_key`; дальше актор пересчитывает его после `set_key` +
         100 мс, до `MdnsTxtChanged`);
       - `ZigbeeHandle::mode` (константа: смена роли = рестарт демона);
       - `ZigbeeHandle::status`: начальная фаза («off» / «starting» / «error: cannot open the
         radio») считается в `spawn`, и первый `Changed` уже лежит в канале, когда `spawn`
         возвращается; дальше статус ведёт актор;
       - `Mixer` синхронный целиком (актора нет);
     - ведёт актор или рабочий поток (после команды — прежнее значение):
       - `PlayerHandle::state` (пишет поток GLib; команды уходят туда через `g_idle_add`);
       - `SendspinHandle::streaming`: меняют колбэки начала/конца потока, обновление группы со
         `stopped` и разрыв, найденный в `poll()`;
       - `ZigbeeHandle::flashing` (запись начинается по ответу радио или таймауту, не по команде);
       - `ApiHandle::{connected, voice_subscribed}`: ошибку записи или переполнение backlog
         («client not reading») видит актор, итог приходит событиями `VoiceStartFailed{run}`,
         `VoiceAudioOverflow{run}`, `VoiceSubscribed(false)` и, возможно, `VoiceSubscribed(true)`
         (голос передан другому клиенту), `ClientDisconnected`. Контроллер не судит о результате
         по геттеру сразу после вызова;
       - `ZigbeeHandle::status` после `join`/`leave`: изменение видно, когда актор обработал
         команду. Контроллер читает `status()` по `Changed` (join шлёт его после изменения) и в
         `on_client_connected`; если `ClientConnected` обработан между `join()` и актором,
         публикуется прежний статус, и следующий `Changed` его исправляет;
     - живое чтение состояния библиотеки: `SendspinHandle::connected` — неблокирующее атомарное
       чтение (`connection_manager_->is_connected()` = `connected_.load(acquire)`, вызывается из
       любого потока); handle зовёт `is_connected` шима синхронно на потоке рантайма, без флага и
       без `spawn_blocking`.
   - События идут через `tokio::sync::mpsc::UnboundedReceiver<XEvent>`. Контроллер сам владеет
     своим состоянием и выбирает события через `tokio::select!`, реентрантных вызовов нет.
   - Ответ на запрос, который можно отменить или вытеснить (воспроизведение, арбитраж, голосовой
     run), несёт id, возвращённый методом handle (`PlayId`, `claim → u64`,
     `voice_request_start → u64`). Устаревшие id контроллер игнорирует. У проверки и установки
     обновлений id только связывает ответ с запросом.
3. Таймеры внутри актора: `pulsar_core::Timers<K>` (ключ — enum актора):
   - на каждом пробуждении берётся снимок просроченных ключей; они срабатывают в порядке
     последнего (пере)взвода;
   - ключ, отменённый или перевзведённый обработкой более раннего ключа того же снимка,
     пропускается;
   - повторяющийся таймер перевзводится на `now + period` до возврата ключа;
   - `next()` cancel-safe;
   - ключи одного снимка срабатывают подряд, без других событий между ними, только если
     `timers.next()` — первая ветка `tokio::select!` с `biased;`.
4. Блокирующие и реалтайм-циклы — отдельные `std::thread` с именем: захват и обработка ALSA,
   GLib main loop, sink-колбэк sendspin-cpp, XMODEM-прошивка радио (последняя может идти и в
   `spawn_blocking`). Наружу они отправляют данные через `UnboundedSender` (Send), команды получают
   через атомики, `Mutex` или `std::sync::mpsc`.
5. Остановка.
   - Актор завершается, когда закрыт его канал команд (все handle уничтожены). При выходе он
     гасит LED, отпускает reset радио, шлёт mDNS goodbye и т. п.
   - Актор без handle (датчик света) завершается, когда отправка события вернула ошибку, то есть
     получатель уничтожен.
   - `main` после выхода контроллера уничтожает handle и ждёт `actors::join_all(3 s)`. К этому
     моменту уже взведён watchdog teardown на 5 с.
   - Потоки останавливаются в `Drop` своего владельца (флаг + join).
   - Исключение — поток записи радио Zigbee: `radio_flash` нельзя прервать, поэтому актор при
     выходе его не ждёт — поток отсоединяется и заканчивается вместе с процессом. Радио остаётся
     без приложения, следующий старт находит его молчащим и прошивает заново.
6. Не держать `RefCell`-borrow через `.await`. Состояние актора хранится по значению в его задаче.
7. Логи пишутся через `log::{debug,info,warn,error}!`, бэкенд — в `pulsar-core`. Записи чужих
   крейтов (target не начинается с `pulsar`) идут только в stderr и только с уровня Warn, в
   ESPHome-sink не попадают. Утилита, чей крейт-бинарь назван не `pulsar*` (`zigbee_probe`,
   `hw_probe`, …), сразу после `init_logger()` вызывает
   `pulsar_core::add_own_target(module_path!())`: её записи (target равен префиксу или лежит под
   `префикс::`) тогда считаются своими — уровень stderr из `PULSAR_LOG_LEVEL` и sink. `Trace`
   отбрасывается. Сообщение обрезается до 1023 байт; повторы схлопываются в «last message
   repeated N times» (не чаще раза в 30 с); очередь sink — 512 строк, при переполнении первой
   идёт «N log lines dropped».
8. `unsafe` и все вызовы `libc` (включая `std::env::set_var`) живут только в модулях
   `sys.rs`/`ffi.rs` и в обработчике крашей.
   - В каждом крейте `#![deny(unsafe_op_in_unsafe_fn)]`, у каждого блока комментарий `// SAFETY:`.
   - Символы dlopen резолвятся один раз в структуру указателей на функции.
   - `gst.rs` в `pulsar-media` — такой же FFI-модуль, как `ffi.rs` (dlopen GLib/GStreamer
     прошивки).
   - Раскладки C-структур — `#[repr(C)]` с compile-time `assert!(core::mem::offset_of!(…) == …)`
     под `#[cfg(target_arch)]`.
   - Обработчик крашей не аллоцирует и не использует `eprintln!`/`std::io`: только
     `libc::write(2, …)`.
9. Комментарии на английском и объясняют факты устройства и ограничения, а не код (конвенция
   AGENTS.md).
10. `make -C satellite check` должен проходить: `cargo fmt --all --check`,
    `cargo clippy --workspace --all-targets --locked -- -D warnings` (host), тот же clippy для
    станций через `cargo-zigbuild clippy … --target aarch64-unknown-linux-gnu.2.36 --target armv7-unknown-linux-gnueabihf.2.30`
    (цели ставит rustup по `rust-toolchain.toml`, линкер не нужен; build-скрипт `ring` компилирует
    C для цели, поэтому нужен zig — любой версии, в CI 0.14.1 — и cargo-zigbuild 0.24.0, с теми же
    `CARGO_ZIGBUILD_*`, что у arch-сборок), `cargo test --workspace --locked` (host x86_64, без
    sendspin-cpp).
    - Clippy линтует host и обе цели. Архитектурно-зависимый код вместе со всеми его хелперами
      целиком лежит под `#[cfg(target_arch = …)]`, чтобы ни на одной из них не было мёртвого кода.
    - FFI-сторона `pulsar-sendspin` (cfg `sendspin_ffi`) линтуется отдельным проходом:
      `CARGO_TARGET_DIR=target/clippy-ffi RUSTFLAGS='--cfg sendspin_ffi' cargo clippy -p pulsar-sendspin --all-targets`
      для обеих целей (только проверка типов, C++ не компилируется и не линкуется).

## Плавающая точка (FMA)

- Rust сам никогда не сливает умножение со сложением. Там, где важно побитовое совпадение
  (пороги, округление до целого, гистерезис, решение о бите), выражение `a + b*c` записано как
  `mul_add` — одно округление вместо двух; результат может отличаться от раздельных операций на
  1–2 ulp (например, кадры `clock_fx` на границах .5). У такого места — комментарий с причиной.
- Расстановка `mul_add` зафиксирована golden-тестами: `pulsar-audio` `tests/spectrum_golden.rs`
  (окно Ханна, `norm`, среднее баса), `pulsar-clock` `clock_fx.rs` (`clock_fx_render.txt`,
  `clock_fx_levels.txt`), `pulsar-hw` `led_fx/tests.rs` (`led_fx.txt`), `pulsar-media`
  `tests/mixer_golden.rs` (кривая `to_legacy`), `pulsar-core` `ambient.rs`
  (`rounds_like_the_fused_cxx_build`). Без теста, но с той же логикой: уровень wake
  (`frontend.rs` `mean_square`), энергии `ww_offline`, матрица цвета корпуса в `leds.rs`,
  диапазоны музыки часов в `clock.rs`, число громкости на часах. Сливания не добавлять и не
  убирать.
- Станционные armhf-сборки дают аппаратный `vfma` для `mul_add` только с
  `-C target-cpu=cortex-a53` (его ставит Makefile, см. «Рецепт линковки» ниже). Сборка armv7 без
  этого флага (например, ручной `cargo build --target armv7-unknown-linux-gnueabihf` без
  `RUSTFLAGS` из Makefile) уводит `mul_add` в вызов libm `fmaf`/`fma`: результат тот же, но
  медленнее; в горячих циклах на каждый сэмпл (аудио, спектр) это заметно. На x86_64-хосте
  `mul_add` — тоже вызов libm.
- `sinf`/`cosf`/`log10f`: armhf-сборка берёт их из compiler_rt zig, aarch64 — из glibc. Они
  иногда расходятся на 1 ulp (5 из 512 значений окна Ханна; на golden-данных спектра — полосы в
  47 из 456 чанков, биты beat не меняются), поэтому сравнение спектра идёт с допуском.

## Рецепт линковки C++ (sendspin-cpp)

Шим `pulsar-sendspin/shim/` и статические библиотеки sendspin-cpp линкуются через `cargo zigbuild`
(cargo-zigbuild 0.24.0, zig 0.14.1, `cc` из workspace) в окружении Makefile:
`RUSTFLAGS='-C relocation-model=static -C force-frame-pointers=yes'` плюс
`-C target-cpu=cortex-a53` для armhf, без `CC_*`/`CXX_*`. Достаточно минимального рецепта:

```rust
// build.rs
cc::Build::new().cpp(true).std("c++20").file("…").compile("…");
println!("cargo:rustc-link-lib=c++");
```

- `c++abi` и `-Wl,--start-group`/`--end-group` не нужны: zig сам линкует свои libc++,
  libc++abi и libunwind статически. Для статических библиотек с перекрёстными ссылками
  `--start-group` остаётся запасным путём.
- `-C target-cpu=cortex-a53` для armhf соответствует `-mcpu=cortex_a53` у C++-части сборки и
  делает `mul_add` аппаратным `vfma` (см. «Плавающая точка»).
- non-PIE: оба бинаря `EXEC`, адреса в отчёте о падении абсолютные. `NEEDED`: aarch64 — только
  `libc.so.6` (максимум `GLIBC_2.34`); armhf — `libpthread.so.0`, `libc.so.6`, `libdl.so.2`,
  `ld-linux-armhf.so.3` (максимум `GLIBC_2.30`), `hard-float ABI`.
- При компиляции `cc` печатает безвредные предупреждения проверки компилятора через обёртки
  cargo-zigbuild: aarch64 — `zig: error: version '.2.36' in target triple 'aarch64-unknown-linux-gnu.2.36' is invalid`,
  armhf — `…/cargo-zigbuild/0.24.0/aeabi_unaligned.c:1:1: error: FileNotFound`. Объект при этом
  собирается и линкуется.

## Заметки о поведении сервисов

### `pulsar-core`

- Строка stderr: `HH:MM:SS.mmm L file:line msg` (локальное время, `L` из `DIWE`, file =
  basename); уровень из `PULSAR_LOG_LEVEL` (0 = Debug, 1 = Info по умолчанию, 2 = Warn,
  3 = Error). Уровень `log` выставляется динамически как самый подробный из уровня stderr и
  уровня sink; notify sink срабатывает только при переходе очереди из пустой в непустую, лог
  изнутри notify — no-op.
- `net::best_interface`: wlan0 = 3 очка, интерфейс маршрута по умолчанию (`/proc/net/route`) = 2,
  остальные = 1; только `IFF_UP|IFF_MULTICAST`, не loopback, не 169.254/16.
  `best_interface_checked` отдаёт `Err`, если `getifaddrs` не удался (mDNS тогда сохраняет
  прежний выбор).
- `tz::set_tz` вызывается только при изменении значения: `set_var("TZ")` гоняется с `getenv` на
  потоках GLib.
- `model::detect_model` читает `/proc/device-tree/compatible` (NUL-разделённый список; первый
  `yandex,cucumber`, `yandex,plum` или `yandex,station-mini2`).

### `state.conf` и откат на `.prev`

`state.conf` побайтно совместим с прежним C++-демоном, потому что `supervise.sh` может откатить
станцию на `.prev` (в том числе на C++-сборку), и оба бинаря читают один файл
(`pulsar/src/settings.rs`). Числа пишутся как `%g` (`pulsar_core::fmt::g6`, 6 значащих цифр) и
читаются как `strtof` (`parse_f32_prefix`), в C-локали потока (`uselocale`), независимо от
`setlocale` где-либо в процессе. Новые ключи добавляются с безопасными значениями по умолчанию.

Запись (`Settings::save`, `StateFile`): весь файл пишется в `<path>.tmp`, затем fsync, rename
поверх `state.conf` и fsync каталога. На Лайт 2 `/data` — ubifs, где rename доходит до флеша
раньше несинхронизированных данных, и без fsync отключение питания могло оставить пустой файл.
Неудачная запись логируется, временный файл удаляется, прежний `state.conf` остаётся, а изменение
считается несохранённым до следующего изменения или flush.

- `Controller::persist()` откладывает запись: файл пишется через 1 с (`SAVE_DELAY_MS`) после
  последнего изменения, так что ползунок или зажатая клавиша громкости дают одну запись. Ветка
  `state.due()` стоит в `select!` контроллера сразу после таймеров.
- `Controller::persist_now()` пишет сразу, отменяет отложенную запись и возвращает, лёг ли файл
  на диск. Так пишутся `update_attempt` (установка начинается; сброс после несмертельной ошибки
  установки), `zigbee_role` вместе со сбросом `zigbee_flash_failed` (перед рестартом),
  `zigbee_flash_failed` после отвергнутого радио образа и выключатель `mic_muted`. Если метку
  `update_attempt` записать не удалось, установка не начинается: прежнее значение возвращается в
  памяти, HA видит её как неудачную, в лог идёт предупреждение.
- Исправления при старте (миграция громкости, неизвестный цвет корпуса, сброс `update_attempt`
  новой версией) и сброс `update_attempt` при выходе посреди установки идут через `persist()`.
- `main` после `run()` один раз вызывает `flush_settings()`, уже под watchdog teardown: зависший
  fsync не мешает выходу, а оборванная запись оставляет прежний файл. Падение или SIGKILL теряют
  изменения последней секунды.
- Новый ключ, который должен пережить рестарт, падение или замену бинаря сразу после изменения,
  пишется через `persist_now()`.

### `pulsar-alsa`

- Для `hw_params` `set_rate_near` / `set_period_size_near` / `set_buffer_size_near` /
  `get_period_size` / `get_buffer_size` сначала ищется `__snd_pcm_hw_params_*`, затем обычное имя:
  в версионированной alsa-lib (образы Миди с armhf userland) обычное имя может привязаться к
  compat-записи ALSA_0.9, которая принимает значения вместо указателей (period/buffer читаются
  как 0, активационная фраза не работает).
- `load()` идемпотентна; первая неудача один раз пишет error с текстом dlerror, дальше `open`
  отдаёт `-ENOENT`.
- `quiet_error_handler()` пишет шум alsa-lib в `log::debug` только при уровне stderr Debug.

### `pulsar-esphome`

- mDNS вынесен из ApiServer: респондер создаёт контроллер, ApiServer только отдаёт `mdns_txt()` и
  событие `MdnsTxtChanged`. IP для сенсора «IP address» берётся из
  `pulsar_core::net::best_interface()`.

### `pulsar-lan`

- mDNS: если 5353 занят без `SO_REUSEADDR`/`SO_REUSEPORT`, респондер берёт эфемерный порт и только
  анонсирует. `add_service`/`set_txt` сразу после `spawn` (до первого запуска актора) уходят в
  стартовый анонс; `ipv4()` они не меняют.
- Арбитр: сокет не открылся → warn, `ok() = false`, `claim(decide = true)` сразу даёт
  `Decided { won: true, peers: 0 }`. `decide = false` — только рассылка (busy-claim при звонке
  таймера). Уже отправленный `Decided` может прийти и после `cancel()`: контроллер забывает свой
  `claim_id` при cancel и сопоставляет события по id.
- `spawn_traced` (только для `lan_probe`): тот же сервис плюс канал отвеченных вопросов mDNS /
  принятых чужих claims.

### `pulsar-audio`

- Ошибка dlopen / VQE / спотера логируется, handle деградированный (событий нет). PCM, который не
  открылся, не считается ошибкой старта: поток захвата пишет ошибку и повторяет открытие каждые
  2 с. Потоки останавливаются в `Drop` последнего клона handle.
- `stats()` / `set_raw_tap()` — только для `frontend_probe`: счётчики потоков и сырой захват для
  `--record`.
- Пре-ролл wake word (switch «Command right after wake word», `wake_preroll=` в `state.conf`, по
  умолчанию включён; `FrontendHandle::set_preroll`, выключенный даёт пустой `preroll`, удержание
  стрима остаётся): спотер подтверждает слово на несколько чанков позже, чем оно кончилось.
  Библиотека описывает и двухстадийный путь (событие 3 `1ST_STAGE_HIT`, верификатор ждёт данных,
  5 — подтверждение, 6 — отказ), на нём держится правило `FirstStage`. Сейчас его не использует
  ни одна модель: обе модели Лайт 2 и «Яндекс» Миди события 3 не выдают (только 2), а у «Алисы»
  Миди событие 3 приходит в одном чанке с подтверждением 5 и ничего не отсчитывает; у всех
  `Fixed`. Поток обработки держит кольцо последних 32 чанков `VQE_OMNI_WITH_GAIN` (1,024 с) и
  кладёт в `FrontendEvent::Wake { preroll }` звук от конца слова до подтверждающего чанка
  включительно. Длина — правило из `Model::wake_preroll` для id модели (имя каталога, `alisa` /
  `yandex`): чанки после 1-й стадии плюс смещение или фиксированное число; id без правила —
  пустой пре-ролл. Значения берутся с недолётом, чтобы само слово не попало в STT (HA его из
  текста не убирает): на Лайт 2 — самый короткий замер по громким словам, на Миди все слова у
  шумового пола и замер не отличает артефакт от короткой задержки, поэтому взято типичное
  значение; окончательно все значения выставлены приёмкой через HA. Каждое срабатывание пишет
  `wake timing: first_stage=<чанки> speech_end=<чанки> preroll=<чанки>` (`speech_end` — сколько
  чанков назад уровень `VQE_OMNI` был не ниже −20 дБ от пика окна). Если имена событий 3 / 6 в
  библиотеке не те (`spotter event table differs` в логе), якорь отключён и действует запасное
  значение правила.
- После `Wake` поток обработки шлёт `Audio`, пока владелец не вызовет `wake_handled()` (счётчик,
  `set_streaming` его не трогает): чанки между подтверждением и `set_streaming(true)` контроллера
  продолжают пре-ролл без дыры. Контроллер вызывает `wake_handled()` ровно раз на `Wake`, после
  `on_wake`; пре-ролл уходит в `start_pipeline` или в `arb_audio` арбитража.

### `pulsar-media`

- `PlayerEvent::Done` приходит ровно один раз на каждый `play()`. Список, закончившийся на потоке
  GLib до `stop()`/`play()`, получает фактический `ok`; вытесненный `stop()`/`play()` список
  получает `false` синхронно из этого вызова.
- Ошибка dlopen / `gst_init` логируется («player init failed»); handle деградированный: каждый
  `play()` → `Done { ok: false }`, `state()` = Idle. Плеер один на процесс: повторный `spawn()`
  логирует ошибку и отдаёт деградированный handle. При старте плеер ставит
  `pulsar_alsa::quiet_error_handler()`.
- `Mixer::open` берёт raw, только если чтение удалось и raw > 0, иначе громкость 0.5; без контрола
  `available() = false`, громкость хранится в памяти. `set_volume`/`set_muted` сразу пишут контрол
  и поля, геттеры читают поля.

### `pulsar-sendspin`

- Без sendspin-cpp (host-сборка, cfg `sendspin_ffi` не задан) `spawn` отдаёт
  `Err("sendspin: built without sendspin-cpp")`.
- Конец потока без `stopped` в состоянии группы удерживается 1 с (sink освобождается сразу):
  перезапуск потока сервером (seek / skip) не дёргает состояние медиаплеера в HA и музыкальные
  дисплеи. `stopped` завершает удерживаемый конец сразу.
- `yield_to_local()` только шлёт `client/leave`, `streaming` синхронно не меняет.

### `pulsar-hw` и `pulsar-clock`

- Ошибка открытия framebuffer / sysfs логируется («LEDs unavailable», «clock unavailable»), вывод
  становится no-op, состояние работает. `set_case_color` синхронно грузит матрицу из
  `/system/vendor/quasar/ledconfigs/<stem>.json` и работает и без открытых LED.
- `spawn_input` отдаёт `Err`, если не открылось ни одно устройство.
- `spawn_light_sensor` отдаёт первое значение сразу.

### `pulsar-zigbee`

- `join()`: актор проверяет фазу Running и роль роутера, затем `ready && !joined && !joining`,
  ставит `joining`, очищает error, шлёт steering, взводит таймер 90 с и отправляет `Changed`.
  `leave()`: при тех же условиях (Running, роутер, ready) сбрасывает `joining` и очищает error,
  без `Changed`.
- Внутренний API (link, coordinator, router, flasher) описан в rustdoc крейта.

### `pulsar-update`

- `check()`/`install()`: `None` = занят, запрос не принят, события не будет; `Some(id)` — id из
  общего счётчика с 1. `busy` ставится CAS в handle до отправки команды; актор сбрасывает его
  непосредственно перед отправкой `Checked`/`Installed`. Окно, когда `busy()` уже false, а событие
  ещё в очереди: для `Installed` его закрывает флаг установки контроллера; для `Checked` —
  контроллер, который не начинает установку, пока id проверки не обработан.
- Когда закрыты все handle (выход демона), установка в процессе отменяется: до `--version` и
  замены бинаря она прерывается (`pulsar.new` удаляется), уже идущая замена завершается.

## Порты и тестовый режим

- Базовые порты: ESPHome 6053, Sendspin 8928, deCONZ 6638, арбитраж 6054.
- `PULSAR_PORT_OFFSET=<u16>` (по умолчанию 0) прибавляется ко всем четырём портам через checked
  add. Если значение не разбирается как u16 или сумма переполняется — лог
  `invalid PULSAR_PORT_OFFSET` и выход с кодом 1 до запуска сервисов.
- При ненулевом offset контроллер не создаёт mDNS-респондер: HA и Music Assistant тестовый
  экземпляр не видят.
- Демон всегда берёт порты из конфигов, хардкода нет.
