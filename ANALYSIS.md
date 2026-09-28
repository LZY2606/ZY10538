# ANALYSIS — Rust log：宏展开、静态限级与全局 logger 调用链

本文追踪一条 `log!` 调用从宏展开到全局 logger 的完整调用链，标注每个短路点的
源码位置，记录编译期裁剪与运行时过滤的分工、格式参数求值时机、全局 logger
初始化状态的设计取舍，以及对应的可重放验证证据。

## 1. 调用链总览（一次 `log!` 的完整路径）

```
log!(Level::Info, "x = {}", probe())
  │  src/macros.rs:75        log!            展开 target/logger 默认值
  ▼
__log!                       src/macros.rs:119
  │  src/macros.rs:137       短路点 S1+S2:
  │                          if lvl <= STATIC_MAX_LEVEL && lvl <= max_level()
  ▼
__private_api::log           src/__private_api.rs:86
  │  src/__private_api.rs:58 log_impl        短路点 S3: 非 kv feature 且带 kv 对 → panic
  │                          Record::builder() … build()   src/lib.rs:907/1038/1143
  ▼
GlobalLogger::log            src/__private_api.rs:41
  │                          logger()        src/lib.rs:1616  短路点 S4: 未初始化 → NopLogger
  ▼
<&'static dyn Log>::log      src/lib.rs:1306（Log trait）
                             已设置 → 用户 logger；未设置 → NopLogger（src/lib.rs:1318）
```

`log_enabled!` 走平行路径：`src/macros.rs:391` → `__log_enabled!`
（`src/macros.rs:415`，同一组 S1+S2 条件）→ `__private_api::enabled`
（`src/__private_api.rs:105`，构造 `Metadata` 并调用 `Log::enabled`）。

## 2. 短路点清单

| 编号 | 位置 | 条件 | 可观察效果 |
| --- | --- | --- | --- |
| S1 | `src/macros.rs:123`、`:137`（`__log!` 两个分支）；`src/macros.rs:415`（`__log_enabled!`） | `lvl > STATIC_MAX_LEVEL`（`src/lib.rs:1646`，由 `max_level_*` / `release_max_level_*` feature 决定的常量） | 参数表达式不求值、不构造 `Record`、不调用 logger；`if` 体为常量假，可被编译器整体消除 |
| S2 | 同上 | `lvl > max_level()`（`src/lib.rs:1431`，读 `MAX_LOG_LEVEL_FILTER`，`src/lib.rs:467`） | 同上，但分支在运行时判定；`set_max_level`（`src/lib.rs:1386`）可随时改变结果 |
| S3 | `src/__private_api.rs:67` | 未开 `kv` feature 却传入 kv 对 | 运行期 panic；宏层在无 `kv` feature 时直接 `compile_error!`（`src/macros.rs:465`、`:523`），S3 是手写 `__private_api::log` 调用的兜底 |
| S4 | `src/lib.rs:1616`（`logger()`） | `STATE != INITIALIZED` | 返回 `&NopLogger`：`enabled` 恒 false、`log`/`flush` 为空操作；不 panic、不阻塞 |

## 3. 全局 logger 初始化状态机

- 状态：`UNINITIALIZED(0) / INITIALIZING(1) / INITIALIZED(2)`，`src/lib.rs:463-465`；
  `LOGGER` 静态（`src/lib.rs:455`）由 `STATE` 保护。
- `set_logger` → `set_logger_inner`（`src/lib.rs:1513`、`:1518`）：
  `compare_exchange(UNINITIALIZED → INITIALIZING, Acquire)` 成功后写入 `LOGGER`，
  再 `store(INITIALIZED, Release)`；此后任何 `set_logger`/`set_logger_racy`
  （`src/lib.rs:1564`）都返回 `SetLoggerError`。**写一次（write-once）**。
- `logger()`（`src/lib.rs:1616`）：`Acquire` 读 `STATE`，未初始化返回 `&NopLogger`，
  已初始化返回 `LOGGER`；读路径无锁。
- 关键边界：**logger 是否设置不影响宏的 S1/S2 判定**。`max_level()` 默认是
  `Off`（`MAX_LOG_LEVEL_FILTER` 初值 0），所以"未设置 logger 且未调
  `set_max_level`"时在 S2 短路；若只调 `set_max_level` 而不设 logger，宏会
  求值参数、构造 `Record` 并 dispatch 到 `NopLogger` 丢弃。

## 4. 格式参数何时真正求值

宏展开后，`format_args!` 位于 S1+S2 的 `if` 体内，因此：

1. S1 或 S2 拒绝 → 参数表达式**零求值**（`probe()` 之类的副作用不会触发）。
2. 两门通过 → 参数表达式**求值一次**（构造 `fmt::Arguments` 时按引用捕获）。
3. `Display::fmt` 的实际渲染发生在 **logger 内部**：记录型 logger 渲染一次；
   `NopLogger` 永不渲染；排队型 logger 可能延迟渲染。`Record<'a>` 以借用方式
   持有参数，生命周期不超出本次 `log!` 调用。

可重放边界用例（全部确定性、不依赖线程调度）：

| 用例 | 位置 | 状态 | 期望 |
| --- | --- | --- | --- |
| 运行时过滤短路 | `tests/recording_logger.rs` `runtime_filter_gates_format_argument_evaluation` | 已设 logger，`set_max_level(Error)` 后 `info!` | probe/Display 计数不变、事件序列 `[]` |
| 运行时放行 | 同上 | `set_max_level(Info)` 后 `info!` | probe +1、Display +1、恰好一条 `Log` 事件 |
| 静态裁剪 | 同上 trace 探针 | `STATIC_MAX_LEVEL < Trace`（如 `max_level_debug`） | trace 的 probe 不求值 |
| logger 未设置 + 默认限级 | `tests/logger_unset.rs` phase 1 | `max_level() == Off` | 参数零求值，`logger().enabled` 为 false |
| logger 未设置 + 提升限级 | `tests/logger_unset.rs` phase 2 | 仅 `set_max_level(Trace)` | probe 求值一次、Display 零渲染（NopLogger 丢弃） |
| 一次初始化 | `tests/recording_logger.rs` `global_logger_initializes_exactly_once` | 初始化后再次 `set_logger`/`set_logger_racy` | 均返回 `SetLoggerError` |

所有依赖静态裁剪的断言都从 `log::STATIC_MAX_LEVEL` 现算期望值，同一测试文件
在 `cargo test`（静态 Trace）与 `max_level_off`（静态 Off）下都确定通过。

## 5. 设计取舍：编译期裁剪 vs 运行时过滤

| 维度 | 编译期裁剪（S1） | 运行时过滤（S2 + `Log::enabled`） |
| --- | --- | --- |
| 机制 | feature → `STATIC_MAX_LEVEL` 常量 | `MAX_LOG_LEVEL_FILTER` 原子量 + logger 的 `enabled` |
| 成本 | 零（死代码消除，含参数求值） | 一次原子 load + 分支；`log_enabled!` 另加一次 `enabled` 调用 |
| 可改性 | 需重新编译；`max_level_*` 与 `release_max_level_*` 各自互斥（`src/lib.rs:379`、`:399` 的 `compile_error!`），release 版仅在 `debug_assertions` 关闭时生效 | `set_max_level` 随时可调；logger 可按 target 细粒度过滤 |
| 分工 | 砍掉"该构建产物永远不需要"的级别 | 承载部署期/运行期的动态 verbosity 决策 |

备选方案与最终取舍：

- **格式参数求值时机**。(a) 现状：宏内门控后求值——零成本，但要求调用者用
  `log_enabled!` 保护昂贵参数；(b) 总是构造 `Record` 交给 logger 过滤——API
  简单但违背 facade 的零成本目标；(c) 惰性闭包参数——改动宏语法、破坏兼容。
  取舍：保持 (a)，用测试把"两门通过才求值"固定为可重放契约。
- **全局 logger 初始化**。(a) 现状：write-once 状态机 + `NopLogger` 兜底——
  读路径无锁、初始化竞态安全、未初始化时行为良定义；(b) 允许运行期替换
  logger——需要 `RwLock` 或原子指针交换，读路径变贵且引入重入复杂度；
  (c) `OnceLock` 直存——等价语义但丢掉三态中对 INITIALIZING 的显式处理。
  取舍：保持 (a)，用负向测试固定"第二次 `set_logger` 必失败"。
- **所有权与资源上限**（公开设计）：logger 以 `&'static dyn Log` 注册，所有权
  不转移、facade 不释放；`Record<'a>`/`Metadata<'a>` 只借用调用点数据，单次
  `log!` 为 O(1) 栈空间，facade 热路径不分配堆内存；kv 对以切片借用传入。
  这些上限保证旧调用者在未开启新行为时观察不到任何兼容性变化。

## 6. 验证证据

- `cargo fetch --locked`：通过（先生成 `Cargo.lock`，其被 `.gitignore` 排除，
  可随时再生成）。
- `cargo test --locked --all-features`：**按设计失败**，非回归。具体状态：
  `--all-features` 同时开启 6 个 `max_level_*` 与 6 个 `release_max_level_*`
  互斥 feature，命中 `src/lib.rs:379`（`multiple max_level_* features set`）
  与 `src/lib.rs:399`（`multiple release_max_level_* features set`）的
  `compile_error!`。这是 crate 的公开契约，因此用下列合法组合做全量验证：
- 全量验证矩阵（全部通过，0 failed）：
  - `cargo test --locked`：静态 Trace；`kv` 关闭，负向测试
    `kv_pairs_without_kv_feature_are_rejected`（should_panic）生效。
  - `cargo test --locked --features max_level_debug`：部分静态裁剪，trace 探针
    证明 S1 短路。
  - `cargo test --locked --features max_level_off`：完全静态裁剪，等价于
    `--all-features` 下 `STATIC_MAX_LEVEL` 的实际取值（Off）。
  - `cargo test --locked --features "std serde kv kv_sval kv_serde kv_std
    kv_unstable kv_unstable_sval kv_unstable_std kv_unstable_serde"`：合法最大
    feature 集，`kv_pairs_are_attached_to_the_record` 证明 kv 元数据到达
    `Record`。
  - `cargo test --locked --release --features release_max_level_info`：release
    静态限级路径。
  - `test_max_level_features/`（既有 crate）：`cargo run` 与
    `cargo run --release` 验证 `max_level_debug` + `release_max_level_info`
    在 debug/release 下的差异。
- 新增测试与证明目标的对应：
  - `tests/recording_logger.rs`：调用次数（probe/Display 计数器）、元数据
    （target/module_path/file/line/message/kv）、一次初始化（负向）、
    `log_enabled!` 短路、`flush` 到达、无 `kv` 时 kv 对被拒（负向）。
  - `tests/logger_unset.rs`：独立进程内全局 logger 从未初始化的边界序列。
- 确定性措施：共享全局态的测试经 `GATE` 互斥串行；期望值由
  `STATIC_MAX_LEVEL` 现算；未初始化场景独占一个测试二进制；`should_panic`
  测试造成的 mutex 中毒通过 `into_inner` 恢复，不影响其余测试的判定。
- 退出后无后台工作或临时产物：测试不 spawn 线程/进程；`target/` 与
  `Cargo.lock` 均在 `.gitignore` 内。
