# 0001 — Induction：独立的宿主↔扩展协议（命名、边界、首个切片）

> **语言：** [English](ADR-0001-induction.md)（主文档） · [中文](ADR-0001-induction.zh-CN.md)

**Status:** Accepted（2026-10-12）；首个切片已落地 —— `induction-protocol`、`induction-host`、`induction-guest`，31 个测试通过（`cargo test --workspace`）。

## Context（背景）

这套工作区里有两个程序需要同一件东西：宿主嵌入**常驻扩展进程**，双方在一条字节通道（常见形态是 stdio）上交换类型化帧、调用与效应，扩展随宿主一起死。扩展就是在配置里指名的一个可执行程序 —— 没有代码上传、没有 introspect 遍、没有代码存储、没有 realm、没有 booth 模型。

两个实现都已存在，词汇表也因此存在了三份：

- **guest 面** —— `effector/crates/bgi-guest`（416 行，只用 serde_json + ciborium：这是唯一一开始就长对了形态的部分）。
- **宿主侧** —— `effector/crates/runtime/src/carrier/exec.rs`（825 行）+ `carrier/session.rs`（372 行），与 WS 运行时、内嵌 steel/python carrier、`bwrap` 策略、nushell fifo adapter，以及那套独立的 bare-`exec` 一次性 carrier 焊在一起。
- **一份裁剪复制** —— `mudra/crates/mudrad/src/host.rs`（292 行），帧是手写的 `serde_json::json!` 字面量。

同一个契约的三种拼法产生了漂移，漂移又产生了任何一侧单独都看不见的 bug（见下文*查实的差异*）。第四处宿主只会产生第四种拼法。

## Decision（决定）

### 1. 独立仓、三个 crate

`~/world/induction`，工作区含 `induction-protocol`、`induction-host`、`induction-guest`。

选独立仓而不是放进 `effector` 的理由：`effector` 是某个特定分布式系统的执行层；这是一个任何 Rust 程序都可以引入的嵌入/传输 ABI。消费方不该为了拿一个帧编解码，就继承 effector 的产品线、发版节奏与 CI 身份。

依赖白名单（由工作区文件明文声明、评审把关）：

- `induction-protocol` —— `serde`、`serde_json`、`ciborium`，仅此，永远如此。
- `induction-guest` —— 上述 + `anyhow`。
- `induction-host` —— protocol + `tokio`（rt/io/process/sync/time）+ `libc`（一次 `prctl` 调用）。

裁决排除：`okm`/`fjall`、任何 `aura-*`、`effector-config`、`steel-core`、`pyo3`、`sandbox-runtime-rs`、`ureq`、`tokio-tungstenite`。进程适配器以外的传输形态与任何沙箱策略都属于**宿主**。

三个 crate 而非单 crate 带 features，是因为依赖重量按半边不同：guest 必须能在没有 tokio、没有 libc 的条件下构建，宿主也不该背上 guest 立面。

### 2. 边界：只做通道与生命周期

在内：帧类型、臂名、tier 词汇、schema 面、编解码声明、带预算的会话 I/O、ctx 臂路由 trait、进程适配器（PDEATHSIG + `kill_on_drop`）、常量退避的 supervise。

在外（裁决）：存储（这里没有 store，因此**这个库不做任何持久化** —— 这是结构性的，不是可配置的）；事件源（宿主提供它想投递的帧：mudra 在自己的 epoch 变化时拉一段日志窗口，别的宿主可以直接从内存投递）；timer 与 queue（协议给出名字 + `HostOps` 钩子，实现属于宿主）；沙箱策略（宿主侧的 spawn 关注点）；代码上传/introspection 存储（预编译可执行程序自己声明 schema —— 这正是"只指名一个可执行程序"在这里成立，而 aura 的上传式 booth 路径需要 introspection 收集器与 blob 存储的原因）。

### 3. 线契约的规范选择（已落地形态的并集，取一种拼法）

- **编解码** —— 父进程通过 `INDUCTION_ENCODING` 声明（`"json"`|`"cbor"`；缺失或未知 → json；坏值永不导致会话失败）。走环境变量而不是 argv：fifo 形态的 spawn spec 传的是作者自己的参数。
- **臂** —— ADR-0043 的 ctx 成员，以点分方法路径逐字采用（`invoke`、`iterate.start/next/dispose`、`store`、`emit`、`timer.register/cancel`、`queue.depth/skip_to_head`、`schema`）。由测试锁定。
- **`host_reply` 错误键** —— 规范为 `error`；mudra 在 Induction 之前的 `err` 拼法在解码侧容忍，使改名窗口内已运行的配对不中断。
- **schema** —— 富形态（`receives` / `wildcard_receives` / `hooks` / `storage`）为规范；扁平 `{"on": […]}` 拼法在解码侧接受。两条声明路径都有类型且都由 guest 应答：加载期握手（`initialize` → `hello`），以及拉取形态（一个 event 为 `interface_schema` 的 `call`，上传期 introspection 需要的也是它）。
- **`invoke` 载荷** —— 规范为已接受的 ctx 定义：嵌套 booth 调用（`{type, key, handler, args}`）。扁平 `{verb, args}` 拼法（mudra 当前的控制面）解码为 `InvokeCall::Verb`；宿主路由它能服务的那种形态，迁移是 mudra 的事，不是协议的事。
- **解码严格性** —— 未知臂、未知 kind、未知形状都是解码失败（版本失配信号），绝不做兜底分派。guest 的不可解码行由宿主显式暴露，而不是被吸收。
- **回复纪律** —— `emit` 不带回复（按契约 fire-and-forget）；其余臂都要应答。宿主缺某个臂时回一个显式错误值，绝不回静默 Null。

### 4. 命名：Induction

判据：名字不含宿主域名词；它必须描述真机制（两个隔离进程之间、有中介、无接触的耦合）；要能承受 `X`/`X-host`/`X-guest` 的家族形态并拥有自己的版本轴；不得与已知标准缩写相撞。

选定：**Induction（感应）** —— 一个回路里的电流在**电气隔离**的另一个回路里感应出电流；两侧保有各自的状态与内存，耦合是有中介的，而由互感（mutual inductance）它是双向的。电气隔离就是进程边界；被感应出的电流在它自己的回路里做功。"不是强耦合"的精确物理是谐振电感耦合（k<1）—— 名字取的是机制，不是学科。

被否候选与理由：

- **BGI（Booth Gateway Interface）** —— 带着 aura 的域名词（Booth），且机制词用错（"Gateway" 在本代码库里指的是*宿主调用解析* —— `aura/crates/engine/src/host_wire.rs` —— 不是传输）。
- **Actuator** —— 是 `effector` 的近义词（在这个工作区里 effector 已占该角色），作为 `effector` 自身的候选名也已被否决过一次；决定性的问题是它命名了错的一方：扩展是决策面，执行效应的是宿主，所以 actuator 落在宿主侧，而不是这个库所在的通道。
- **Electromagnetism / Lorentz force** —— 一个是学科名、一个是定律名，都不描述耦合层；"场"已经是 aura 的隐喻（aura = 场，effector = 场中把决策变成作用的部件），用一个宿主中立的库去承载它，等于把 BGI 的错换个喻词重犯；而且物理现象名对一个正在找扩展宿主的读者是无法检索的。
- **Relay** —— 器件层（又一次命名错方），单向（线圈 → 触点）而通道是双向的，且 "relay" 在 aura 自己的测试里已经是 booth 类型名。
- **Yoke** —— 已被 Rust crate 占用（`yoke`，ICU4X；就躺在 aura 的 Cargo.lock 里）。
- **Residency** —— 描述的是生命周期，而承重性质是有中介的连接、控制与操作。
- **Scion / Symbiont** —— 植物/生物语域：耦合层读起来像姿态而不是机制。

### 5. 改名清单（线可见项优先）

在词汇表对外部 guest 冻结之前：

1. `BGI_ENCODING` → `INDUCTION_ENCODING`（carrier 写入侧 + guest 读取侧）。
2. carrier/语言值 `"bgi"` → `"induction"` —— **不是**一个装饰性字符串：`PersistedBooth.language` 是持久字段（`aura/crates/realm/src/meta.rs:176`，12 处注册点）。既然 meta store 里没有真实数据（用户裁决 2026-10-12），走删库重建政策，与 mudra 的原型 store 一致。
3. 错误串（`"bgi/nu adapter: …"`）、fixture/示例二进制名（`bgi_loop`、`bgi_demo`）。
4. 帧的 discriminator（`initialize`、`hello`、`call`、`result`、`host`、`host_reply`）不含 `bgi` —— 无需改动（已核实）。

**Status（2026-10-13 补记）**：四项均已落地。内部 Rust 标识符在后续一轮中
也改了：`BgiSession` → `InductionSession`、`BgiKind` → `SessionKind`、
`bgi_*`/`nu_bgi_*` 测试名 → `induction_*`/`nu_induction_*`（effector runtime
+ aura engine 测试；门：effector `--features python,steel` 41/0 + clippy 0，
aura workspace 70/0 / `--features python` 86/0 + clippy 0）。刻意不做的只剩
ADRs/PLANs 里的叙述扫除——历史裁决正文不改写
（`partition key → instance key` 先例），引用旧名的历史段落原样保留——
它们描述的是当日的代码。

## 实现过程中查实的差异（每条附站点）

以下都是事实，不是计划 —— 都在已落地的代码里核过：

1. **`host_reply` 错误键不一致。** mudra 的宿主写 `{"err": …}`（`mudrad/src/host.rs`），而 carrier 与 guest shim 读 `"error"`（`carrier/exec.rs`、`bgi-guest/src/lib.rs`）。动词失败到 bgi-guest 扩展那里表现为 "host_reply carried neither ok nor error"。把词汇表收束到一处，这一类 bug 就不可能发生；容忍解码覆盖了迁移窗口。
2. **握手是单边的。** `initialize`/`hello` 只存在于 mudra；aura/effector 从不发它，schema 一律通过 `interface_schema` 调用取。现在两种形态都在协议里，已落地的 guest 两种都答。
3. **`invoke` 载荷形状不一致**（嵌套 booth 调用 vs 扁平 verb）。两种都能解码；规范形态是 aura 的 ctx 定义。
4. **bgi-guest 的 iterate producer 绑定不可达。** `pull()` 查一个名为 `__next__` 的 handler，而 `on()` 断言 `__` 前缀的名字是保留名 —— 因此没有任何 API 调用能绑上 producer。只有真配对测试才看得见：`induction-guest` 增加了 `.producer(handler)`，与 `.hook()` 并列的自有 builder（流机制不是生命周期钩子）。
5. **mudra 的事件载荷是宿主约定**，不是线词汇：它把日志行包成 `{event_id, at, snapshot}`。记在这里，免得 mudra 迁移时被误当成协议。
6. **搭测试时踩到的 cargo 坑**：两个包声明同名 bin 时，`CARGO_BIN_EXE_<name>` 解析到了另一个包的二进制（宿主的 live 测试当时静默跑的是 guest crate 的 fixture）。现在 fixture 的 bin 名互不相同（`raw_guest` vs `loop_guest`）。

## Consequences（影响）

- `mudra`（批次 2）用手写帧路径换成 `induction-protocol` + `induction-host`，与 store 耦合的事件拉取留在 `mudrad`，并把错误键切到 `error`。
- `effector`（批次 1）把 guest shim 迁出为 `induction-guest`，并在帧与编解码枚举上采用 `induction-protocol`。
- `aura`（批次 3，是重构不是替换）让 realm 的进程 carrier 分支委托给 `induction-host`。它的 WS 远程 effector 腿留在 aura，作为同一份会话代码之上的另一种传输；它自己的整合问题 —— `effector-protocol::HostOp`（WS/tool 契约）与 `induction-protocol` 的 host 帧 —— **在本文显式留为未决**：pipe 腿的词汇表现在有了唯一的家，而把两种宿主调用类型收口是那一批次要做的决定。
- 本工作区的历史照旧保留：ADR 的历史正文不改写，现行文本更新，改名附一条术语注（`probe → effector` 的先例）。

## Not decided here（本文未决）

- 一次性 bare `exec` carrier（按设计是另一套协议）是否有一天并入本仓。
- 是否引入 `VirtualStorage` 式的 tier/backlog 词汇：这个库没有 store，所以 `queue.*` 只是协议名字，由*拥有队列的宿主*来服务。
- `receives` 的每 handler 对象目前是空对象；它是 tier/deadline 声明的预留位置，在 guest 需要声明它们之前，这仍是宿主策略。