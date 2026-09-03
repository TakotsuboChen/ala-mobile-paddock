//! 运行日志内存环形缓冲（管理端"日志"页·运行日志视图数据源）。
//! 原理：tracing_subscriber 的 MakeWriter 组合器让同一条已格式化日志
//! （终端黑框框形态，含完整技术细节）同时写 stdout 与本缓冲——
//! 不改任何埋点代码，天然全量（tower_http 请求日志、payload 原文、错误堆栈全在内）。
//! 读取走游标增量：/admin/api/runtime-logs?after=<seq> 只返回 seq 之后的行。
//! 缓冲上限 2000 行，超出丢最旧（环形语义）；跨重启清零（纯内存，stdout 才是持久面）。

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

/// 行容量：单管理员调试场景绰绰有余（2000 行 × ~200B ≈ 400KB 内存）。
const CAPACITY: usize = 2000;

struct Ring {
    /// 已写入的总行数（单调递增，作游标 seq；从 1 起，0 表示"从头拉"）
    seq: u64,
    lines: VecDeque<(u64, String)>,
}

static RING: OnceLock<Mutex<Ring>> = OnceLock::new();

fn ring() -> &'static Mutex<Ring> {
    RING.get_or_init(|| {
        Mutex::new(Ring {
            seq: 0,
            lines: VecDeque::with_capacity(CAPACITY),
        })
    })
}

/// 追加一行（fmt 层按行调用 write；空行丢弃）。锁中毒按日志丢失处理（不可恢复场景）。
fn push(line: String) {
    let mut r = match ring().lock() {
        Ok(r) => r,
        Err(_) => return,
    };
    if line.trim().is_empty() {
        return;
    }
    r.seq += 1;
    let seq = r.seq;
    r.lines.push_back((seq, line));
    while r.lines.len() > CAPACITY {
        r.lines.pop_front();
    }
}

/// 游标读取：after=0 从缓冲内最旧行开始，after=N 返回 seq>N 的行。
/// 返回 (行文本列表, 最新 seq)——seq 由调用方透传给下一轮轮询。
pub fn read_after(after: u64) -> (Vec<String>, u64) {
    let r = match ring().lock() {
        Ok(r) => r,
        Err(_) => return (Vec::new(), 0),
    };
    let lines: Vec<String> = r
        .lines
        .iter()
        .filter(|(seq, _)| *seq > after)
        .map(|(_, line)| line.clone())
        .collect();
    (lines, r.seq)
}

/// MakeWriter 适配器：作为 .with_writer(io::stdout.and(RuntimeLogWriter)) 的第二路。
/// 注意：and() 组合下 fmt 层对每个 writer 各写一份——本 writer 只入环，
/// 终端输出由第一路 io::stdout 负责，这里绝不能再透传 stderr（会双重输出）。
pub struct RuntimeLogWriter;

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for RuntimeLogWriter {
    type Writer = RingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        RingWriter
    }
}

/// 把 fmt 层的写入按行切分进环。
pub struct RingWriter;

impl Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // fmt 层一次 write 通常就是一整条事件（自带尾随 \n），按行拆开逐条入环
        for line in String::from_utf8_lossy(buf).lines() {
            push(line.to_string());
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
