# induction（中文）

在宿主程序中嵌入**驻留扩展进程**的独立协议：宿主指定可执行文件、spawn、
握手、投递类型化帧，并服务扩展回调的 ctx 臂。扩展与宿主同生共死
（`PDEATHSIG`），持有自己的状态与内存，与宿主以进程边界相隔——耦合是被
中介的，不是共享的。

这里没有任何持久化，也不是调度器、沙箱或服务注册表：事件、存储、策略都
归宿主所有。命名与边界裁决、以及这套 crate 被抽出所要终结的分歧，见
`docs/ADR-0001-induction.md`。（英文版 README 为权威版本；本文件是其
中文对照。）

## Crates

| crate | 承载什么 | 依赖 |
| --- | --- | --- |
| `induction-protocol` | 帧、ctx 臂名、tier 词汇、schema 面、codec 声明 | `serde`、`serde_json`、`ciborium` |
| `induction-host` | 带预算的会话 I/O、ctx 臂路由（`HostOps`）、进程适配器（PDEATHSIG + `kill_on_drop`）、常量退避 supervisor | protocol + `tokio` + `libc` |
| `induction-guest` | Rust guest 面：`Guest` builder、typed `Ctx`、主循环、分发、schema 应答 | protocol + `serde_json` + `ciborium` + `anyhow` |

## 最小宿主

```rust
use induction_host::{Budgets, HostOps, InvokeCall, OpsFuture, Spec, run_driver, spawn};
use serde_json::json;

struct Verbs;

impl HostOps for Verbs {
    fn invoke<'a>(&'a self, call: &'a InvokeCall) -> OpsFuture<'a> {
        let call = call.clone();
        Box::pin(async move {
            Ok(match call {
                InvokeCall::Verb { verb, args } => json!({ "verb": verb, "args": args }),
                InvokeCall::Booth { handler, .. } => json!({ "handler": handler }),
            })
        })
    }
}

async fn drive(spec: Spec) {
    let mut process = spawn(&spec).expect("spawn the extension");
    if process.session.handshake(Budgets::default().handshake).await.is_none() {
        process.kill();
        return;
    }
    let (_tx, mut rx) = tokio::sync::mpsc::channel(16);
    run_driver(&mut process.session, &Verbs, &mut rx, Budgets::default()).await;
    process.kill();
}
```

## 最小 guest

```rust
use induction_guest::{Ctx, Guest};
use serde_json::{Value, json};

fn main() -> anyhow::Result<()> {
    Guest::new()
        .named("echo")
        .on("echo", |_ctx: &mut Ctx, args: Value| Ok(json!({ "echo": args })))
        .producer(|_ctx: &mut Ctx, args: Value| {
            let round = args.get("round").and_then(Value::as_u64).unwrap_or(0);
            Ok(json!({ "done": false, "item": round }))
        })
        .run()
}
```

`cargo build --example loop -p induction-guest` 构建一个可用的 guest
（`crates/induction-guest/examples/loop.rs`），展示握手、一次 ctx 往返、
iterate 生产者与 lifecycle hook。

## 协议实际携带什么

- **进程生命周期**（`induction-host/src/process.rs`）：host 按声明 spawn 可执行
  （`Spec`：路径 + env）。`PDEATHSIG=SIGTERM` 走 `pre_exec`——host 被 SIGKILL
  也照样带走子进程；常规路径由 `kill_on_drop` 覆盖。stdin/stdout 管道化，
  **stderr 透传**——guest 自己的日志直达 host 终端，不骑帧协议。
- **握手**（`schema.rs`）：host 写 `initialize` 帧（携带 `PROTOCOL` 版本号；
  无协商——词汇表静态、两侧一同安装），guest 以 `Hello` 应答并携带 schema
  （`receives` / `wildcard_receives` / `hooks` / `storage`）。schema 随时可拉：
  向 `interface_schema` 发一次 `call` 即可。
- **帧词汇**（`frame.rs`）：parent → child
  `{"id","kind","event","args"}`，kind ∈ `call | iterate_start |
  iterate_next | iterate_dispose`（iterate 三种额外带 `op` + `stream_id`）；
  child → parent 两个方向——guest 以 `{"host":{"type":<arm>,…}}` 调 ctx 臂，
  host 以 `{"host_reply":{"ok"|"error"}}` 应答；guest 对 host 请求的应答是
  `{"id","result"}`。臂 = ADR-0043 ctx 成员的点分方法路径
  （invoke/store/store.frame/timer.*/queue.*/iterate.*/emit/schema）。
- **信封**：`{item, done:false}` / `{done:true, value?}`。普通调用就是首应答
  即终止的流（ADR-0036）——两种模式共用一条线路形状，流关联从 `done` 导出，
  不靠 `stream_id` 字段。
- **codec 声明**（`encoding.rs`）：`INDUCTION_ENCODING` 环境变量
  （`json` | `cbor`；缺失或未知回退 json——坏值绝不弄垮会话），一会话一
  codec、从不逐帧协商。声明走 env 不走 argv：fifo 形 spawn spec 只传作者
  自己的参数，运行时追加 argv 会破坏作者设定的 arity。
- **预算与驱动**（`session.rs`、`supervise.rs`）：握手与写帧恒带预算
  （默认 5s；写停滞 = 帧未落地——host 持有 cursor 时不得前进）。读分两种
  形态是刻意的：握手与探测带预算读，驱动循环**不带**——流式 guest 可以
  名正言顺地长时间静默，卡死的 peer 只卡死自己。select 循环双活源
  （host 事件帧 + child stdout）；写失败即结束会话，不静默丢帧。
  常量退避 supervisor 收尸并重启。
- **guest 面**（`induction-guest`）：`Guest` builder（`.on` / `.on_wildcard` /
  `.hook` / `.storage`——声明即 schema，相位门是结构性的：run 之后注册
  不可表达）、与 ctx 臂 1:1 镜像的 typed `Ctx`、生产者 guard 表
  （耗尽即终态——生产者写 `done`）。
- **host 面**：`HostOps` trait 与 `route_frame`——codec 与 transport 无关的
  臂分发，单一归属，每个 host 在其上塑自己的 codec（WS id 配对、管道
  带内）。

## stdout 可以是二进制吗？（CBOR）

可以——CBOR 是已落地的编码，不是愿景。`CborCodec` 把 `ciborium` 文档直写
stdout，stdin 同法读。CBOR **自定界**：每个文档自带长度信息，解析器知道
文档在哪结束——不需要换行分帧（那是 JSON 模式的装置），也不需要长度前缀，
文档连着写即可。host 声明 `Encoding::Cbor`（经 env 传给 child），两侧同
codec。对高频小帧流（token delta），CBOR 免去逐帧 JSON 的引号与转义开销。

## Tests

```
cargo test --workspace
```

33 项测试加一个 doctest：wire 契约（decode 严格性、臂词汇、应答形状、schema
形态）、内存会话行为（握手、预算、写停滞、driver 结局）、两个真子进程测试
（spawn + PDEATHSIG + 握手 + 往返 + ctx 路由 + iterate 轮次），以及最小宿主的
doctest。

真子进程测试 spawn 真实 child。各住在拥有 fixture 二进制的包里——cargo
为包的集成测试构建自己的 bins 并导出 `CARGO_BIN_EXE_<name>`，无需手动构建
步骤，测试也永远不会骑过期的 fixture。fixture bin 名跨包唯一
（`induction-host` 的 `raw_guest`、`induction-guest` 的 `loop_guest`）：
两个包的同名 bin 曾让 `CARGO_BIN_EXE` 拿错二进制。

## panic 面

工作区直接 deny 这些逃生通道，不指望 review 兜住。写在根 manifest，经各成员的
`[lints] workspace = true` 生效：

```
clippy::unwrap_used       clippy::expect_used         clippy::panic
clippy::todo              clippy::unimplemented       clippy::unreachable
clippy::unwrap_in_result  clippy::panic_in_result_fn  clippy::indexing_slicing
```

每条 lint 实际覆盖什么，决定这道闸门是墙还是帘：`panic` 只抓 `panic!` ——
`todo!`、`unimplemented!`、`unreachable!` 是三条独立 lint，而 `assert!` 没有
任何 clippy lint 看得见；`unwrap_or*` 不报警，因为它不会 panic。测试保留这套
词汇：单元测试靠每个 crate root 的一行 `#![cfg_attr(test, allow(..))]`，
`tests/` 文件在各自文件根写同一份清单 —— `tests/` 文件是独立 crate，那里的
`cfg(test)` 为假。

于是生产代码里只剩两处敞口，且都是写明的，不是等崩溃转储再发现：

- **`induction-guest` builder 里的三处 `assert!`** —— `on` 拒绝 `__` 前缀
  事件，`on_wildcard` 要求 `prefix.*` 形态，`hook` 只收 `__sleep`/`__wake`。
  输入是扩展作者自己的注册代码，所以它们是带消息的启动失败，不是运行期错误
  路径。没有 lint 管得住（见上），而把 builder 链改成 fallible 会让每个扩展
  为一次 typo 背上 `?` —— 这处按 review 规则保留。
- **一处 `unsafe` 块** —— `pre_exec` 里的 `prctl(PR_SET_PDEATHSIG)`，宿主死了
  不留孤儿子进程。`unsafe_code = "deny"` 是全工作区的，唯一站点带注释的
  `#[allow(unsafe_code)]`。

其余表面都是翻译：`CallFrame::iterate` 收 `IterateKind` 而不是 `FrameKind`，
`call` 形状的流帧因此不可表达，而不是靠 debug 断言在 release 里消失；wire
构造器逐字段填对象，不再对 `Value` 做索引。

## Status

抽取已完成。已落地：protocol/host/guest 三个 crate（批次 1/1b/1c）；
`effector` 的帧与 codec 枚举骑 `induction-protocol`，宿主帧经 `route_frame`
分发器路由（批次 3）；`mudra` 的宿主重基于 `induction-protocol` +
`induction-host`，与 store 耦合的事件拉取留在 `mudrad`（批次 2）；aura 的
WS 远程 effector 腿零改动——`HostOp` 与管道 `HostFrame` 是同一臂词汇的两种
传输编码，共享的是分发器而不是会话（ADR-0001 §Consequences 已裁决）。内部
Rust 标识符在后续一轮中改名（`BgiSession` → `InductionSession`、
`BgiKind` → `SessionKind`、`bgi_*`/`nu_bgi_*` 测试名 → `induction_*`）；
引用旧名的历史 ADR/PLAN 段落原样保留。
