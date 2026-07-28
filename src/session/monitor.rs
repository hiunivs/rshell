//! 系统监控:远程主机通过一条 SSH exec 命令采集 /proc、ps、df 并解析;
//! 本机用 sysinfo。解析函数为纯函数,配单元测试。

use std::time::Instant;

/// 一轮采集结果
#[derive(Clone, Default, Debug)]
pub struct SystemMetrics {
    pub cpu_percent: f32, // 0..100
    pub mem_total_kb: u64,
    pub mem_used_kb: u64,
    pub swap_total_kb: u64,
    pub swap_used_kb: u64,
    pub load: [f32; 3],
    pub uptime_secs: u64,
    pub procs: Vec<ProcInfo>,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    pub disks: Vec<DiskInfo>,
    pub rtt_ms: u32,
    // ---- 以下为详细系统信息页字段 ----
    /// "Ubuntu 24.04.4 LTS"(/etc/os-release PRETTY_NAME / sysinfo long_os_version)
    pub os_pretty: String,
    /// "Linux" / "Darwin"
    pub kernel_name: String,
    /// "6.8.0-136-generic"
    pub kernel_release: String,
    /// "x86_64" / "aarch64"
    pub arch: String,
    pub hostname: String,
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub cpu_mhz: f64,
    pub cpu_cache_kb: u64,
    pub cpu_bogomips: f64,
    /// CPU 占用分类(一轮增量占比,0..100);本机 sysinfo 拿不到 → None
    pub cpu_busy: Option<CpuBusy>,
    /// 每网卡计数与速率(含 lo)
    pub nets: Vec<NetIfInfo>,
}

/// CPU 占用分类(占一轮总增量的百分比)
#[derive(Clone, Default, Debug)]
pub struct CpuBusy {
    pub user: f32,
    pub system: f32,
    pub nice: f32,
    pub idle: f32,
    pub iowait: f32,
    pub irq: f32,
    pub softirq: f32,
    pub steal: f32,
}

#[derive(Clone, Debug)]
pub struct NetIfInfo {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_bps: f64,
    pub tx_bps: f64,
}

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub rss_kb: u64,
    pub cpu: f32,
    pub command: String,
}

#[derive(Clone, Debug)]
pub struct DiskInfo {
    /// 文件系统/设备名(tmpfs、/dev/vda1、zfs 池…)
    pub fs: String,
    pub path: String,
    pub used_kb: u64,
    pub avail_kb: u64,
    pub total_kb: u64,
}

impl DiskInfo {
    pub fn usage_frac(&self) -> f32 {
        if self.total_kb == 0 {
            0.0
        } else {
            self.used_kb as f32 / self.total_kb as f32
        }
    }

    /// 是否真实设备(侧栏只显示真实的;详细页显示全部含 tmpfs)
    pub fn is_real(&self) -> bool {
        const SKIP: &[&str] = &["tmpfs", "devtmpfs", "overlay", "squashfs", "udev", "none"];
        !SKIP.iter().any(|s| self.fs.starts_with(s)) && !self.fs.starts_with("/dev/loop")
    }
}

// ---------------------------------------------------------------------------
// 远程采集
// ---------------------------------------------------------------------------

/// 一次 exec 拿全部数据,段间用 =XXX 分隔
pub const MONITOR_CMD: &str = concat!(
    "echo =STAT; grep '^cpu ' /proc/stat;",
    "echo =LOAD; cat /proc/loadavg;",
    "echo =UP; cat /proc/uptime;",
    "echo =MEM; cat /proc/meminfo;",
    "echo =NET; cat /proc/net/dev;",
    "echo =PS; ps -eo rss,pcpu,comm 2>/dev/null;",
    "echo =DF; df -kP 2>/dev/null;",
    "echo =OS; grep '^PRETTY_NAME=' /etc/os-release 2>/dev/null;",
    "echo K=$(uname -s); echo R=$(uname -r); echo M=$(uname -m); echo H=$(hostname);",
    "echo =CPUINFO; LC_ALL=C lscpu 2>/dev/null"
);

/// 跨采样保存的计数器(CPU 分项/网络分网卡算增量需要上一轮)
#[derive(Default)]
pub struct MonitorState {
    prev_cpu: Option<[u64; 8]>,
    prev_net: Option<(NetCounters, Instant)>,
}

type NetCounters = Vec<(String, u64, u64)>;

impl MonitorState {
    /// 解析一轮远程输出,返回指标
    pub fn update(&mut self, text: &str, rtt_ms: u32) -> SystemMetrics {
        let mut m = SystemMetrics {
            rtt_ms,
            ..Default::default()
        };
        let mut section = "";
        let mut cur_nets: Vec<(String, u64, u64)> = Vec::new();
        for line in text.lines() {
            match line {
                "=STAT" => section = "STAT",
                "=LOAD" => section = "LOAD",
                "=UP" => section = "UP",
                "=MEM" => section = "MEM",
                "=NET" => section = "NET",
                "=PS" => section = "PS",
                "=DF" => section = "DF",
                "=OS" => section = "OS",
                "=CPUINFO" => section = "CPUINFO",
                _ => match section {
                    "STAT" => {
                        if let Some(cur) = parse_proc_stat(line) {
                            if let Some(prev) = self.prev_cpu {
                                let dt: u64 = cur
                                    .iter()
                                    .zip(prev.iter())
                                    .map(|(c, p)| c.saturating_sub(*p))
                                    .sum();
                                if dt > 0 {
                                    let d = |i: usize| {
                                        cur[i].saturating_sub(prev[i]) as f64 / dt as f64 * 100.0
                                    };
                                    m.cpu_percent = (100.0 - d(3) - d(4)).max(0.0) as f32;
                                    m.cpu_busy = Some(CpuBusy {
                                        user: d(0) as f32,
                                        nice: d(1) as f32,
                                        system: d(2) as f32,
                                        idle: d(3) as f32,
                                        iowait: d(4) as f32,
                                        irq: d(5) as f32,
                                        softirq: d(6) as f32,
                                        steal: d(7) as f32,
                                    });
                                }
                            }
                            self.prev_cpu = Some(cur);
                        }
                    }
                    "LOAD" => m.load = parse_loadavg(line).unwrap_or(m.load),
                    "UP" => m.uptime_secs = parse_uptime(line).unwrap_or(m.uptime_secs),
                    "MEM" => parse_meminfo_line(line, &mut m),
                    "NET" => {
                        if let Some((name, rx, tx)) = parse_net_dev_line(line) {
                            cur_nets.push((name, rx, tx));
                        }
                    }
                    "PS" => {
                        if let Some(p) = parse_ps_line(line) {
                            m.procs.push(p);
                        }
                    }
                    "DF" => {
                        if let Some(d) = parse_df_line(line) {
                            m.disks.push(d);
                        }
                    }
                    "OS" => parse_os_line(line, &mut m),
                    "CPUINFO" => parse_lscpu_line(line, &mut m),
                    _ => {}
                },
            }
        }
        // 网络:分网卡速率 = 计数增量 / 时间差;聚合速率不含 lo
        let now = Instant::now();
        let dt = self
            .prev_net
            .as_ref()
            .map(|(_, t)| now.duration_since(*t).as_secs_f64())
            .unwrap_or(0.0);
        for (name, rx, tx) in &cur_nets {
            let (rx_bps, tx_bps) = match &self.prev_net {
                Some((prev, _)) if dt > 0.01 => {
                    let (prx, ptx) = prev
                        .iter()
                        .find(|(n, _, _)| n == name)
                        .map(|(_, r, t)| (*r, *t))
                        .unwrap_or((*rx, *tx));
                    (
                        rx.saturating_sub(prx) as f64 / dt,
                        tx.saturating_sub(ptx) as f64 / dt,
                    )
                }
                _ => (0.0, 0.0),
            };
            m.nets.push(NetIfInfo {
                name: name.clone(),
                rx_bytes: *rx,
                tx_bytes: *tx,
                rx_bps,
                tx_bps,
            });
        }
        m.net_rx_bps = m
            .nets
            .iter()
            .filter(|n| n.name != "lo")
            .map(|n| n.rx_bps)
            .sum();
        m.net_tx_bps = m
            .nets
            .iter()
            .filter(|n| n.name != "lo")
            .map(|n| n.tx_bps)
            .sum();
        self.prev_net = Some((cur_nets, now));
        m
    }
}

/// "cpu  4705 356 584 369291 1642 19 423 0 0 0"
/// → [user, nice, system, idle, iowait, irq, softirq, steal]
pub fn parse_proc_stat(line: &str) -> Option<[u64; 8]> {
    let mut it = line.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let v: Vec<u64> = it.filter_map(|s| s.parse().ok()).collect();
    if v.len() < 8 {
        return None;
    }
    let mut a = [0u64; 8];
    a.copy_from_slice(&v[..8]);
    Some(a)
}

/// "0.01 0.04 0.00 1/234 5678" → [0.01, 0.04, 0.00]
pub fn parse_loadavg(line: &str) -> Option<[f32; 3]> {
    let mut it = line.split_whitespace();
    Some([
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ])
}

/// "35873212.41 71746148.10" → 秒
pub fn parse_uptime(line: &str) -> Option<u64> {
    line.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|f| f as u64)
}

fn parse_meminfo_line(line: &str, m: &mut SystemMetrics) {
    let Some((key, rest)) = line.split_once(':') else {
        return;
    };
    let kb: u64 = match rest.split_whitespace().next().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return,
    };
    match key {
        "MemTotal" => m.mem_total_kb = kb,
        "MemAvailable" => m.mem_used_kb = m.mem_total_kb.saturating_sub(kb),
        "SwapTotal" => m.swap_total_kb = kb,
        "SwapFree" => m.swap_used_kb = m.swap_total_kb.saturating_sub(kb),
        _ => {}
    }
}

/// "  eth0: 123 0 0 0 0 0 0 0 456 0 ..." → (接口名, rx_bytes, tx_bytes);含 lo
pub fn parse_net_dev_line(line: &str) -> Option<(String, u64, u64)> {
    let (iface, rest) = line.split_once(':')?;
    let name = iface.trim().to_string();
    let mut it = rest.split_whitespace();
    let rx: u64 = it.next()?.parse().ok()?;
    let tx: u64 = it.nth(7)?.parse().ok()?; // 跳过 rx 其余 7 个字段后是 tx_bytes
    Some((name, rx, tx))
}

/// "/dev/vda1  41152832 10381920 29069488  27% /" → DiskInfo;
/// 表头返回 None;虚拟文件系统(tmpfs/overlay 等)也保留,由显示层过滤
pub fn parse_df_line(line: &str) -> Option<DiskInfo> {
    let mut it = line.split_whitespace();
    let fs = it.next()?;
    if fs == "Filesystem" {
        return None;
    }
    let total_kb: u64 = it.next()?.parse().ok()?;
    let used_kb: u64 = it.next()?.parse().ok()?;
    let avail_kb: u64 = it.next()?.parse().ok()?;
    let _cap = it.next();
    let path = it.next().unwrap_or("?").to_string();
    Some(DiskInfo {
        fs: fs.to_string(),
        path,
        used_kb,
        avail_kb,
        total_kb,
    })
}

/// =OS 段:'PRETTY_NAME="..."' / K= / R= / M= / H=
fn parse_os_line(line: &str, m: &mut SystemMetrics) {
    if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
        m.os_pretty = v.trim_matches('"').to_string();
    } else if let Some(v) = line.strip_prefix("K=") {
        m.kernel_name = v.to_string();
    } else if let Some(v) = line.strip_prefix("R=") {
        m.kernel_release = v.to_string();
    } else if let Some(v) = line.strip_prefix("M=") {
        m.arch = v.to_string();
    } else if let Some(v) = line.strip_prefix("H=") {
        m.hostname = v.to_string();
    }
}

/// =CPUINFO 段:lscpu 的 "Key: Value" 行(宽容解析,缺字段保持默认)
fn parse_lscpu_line(line: &str, m: &mut SystemMetrics) {
    let Some((key, val)) = line.split_once(':') else {
        return;
    };
    let val = val.trim();
    match key.trim() {
        // `Model` 是处理器的数字型号（如 143），不能覆盖具体的品牌型号名称。
        "Model name" => m.cpu_model = val.to_string(),
        "CPU(s)" => m.cpu_cores = val.parse().unwrap_or(0),
        // 有的系统只有 max/min,取先出现的非零值
        "CPU MHz" | "CPU max MHz" => {
            if m.cpu_mhz == 0.0 {
                m.cpu_mhz = val.parse().unwrap_or(0.0);
            }
        }
        "L3 cache" | "L3" => m.cpu_cache_kb = parse_size_kb(val),
        "BogoMIPS" => m.cpu_bogomips = val.parse().unwrap_or(0.0),
        _ => {}
    }
}

/// "22528 KB" / "22.5 MiB" / "512K" / "1G" → KB
fn parse_size_kb(s: &str) -> u64 {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let n: f64 = s[..split].parse().unwrap_or(0.0);
    let unit = s[split..].trim().to_uppercase();
    let mult = if unit.starts_with('G') {
        1024.0 * 1024.0
    } else if unit.starts_with('M') {
        1024.0
    } else {
        1.0
    };
    (n * mult) as u64
}

/// "  43628  2.0 AliYunDun" → ProcInfo;表头行返回 None
pub fn parse_ps_line(line: &str) -> Option<ProcInfo> {
    let mut it = line.split_whitespace();
    let first = it.next()?;
    if first == "RSS" {
        return None;
    }
    let rss_kb: u64 = first.parse().ok()?;
    let cpu: f32 = it.next()?.parse().ok()?;
    let command = it.next().unwrap_or("?").to_string();
    Some(ProcInfo {
        rss_kb,
        cpu,
        command,
    })
}

// ---------------------------------------------------------------------------
// 本机采样(sysinfo)
// ---------------------------------------------------------------------------

pub struct LocalSampler {
    sys: sysinfo::System,
    disks: sysinfo::Disks,
    networks: sysinfo::Networks,
    last: Instant,
}

impl Default for LocalSampler {
    fn default() -> Self {
        Self {
            sys: sysinfo::System::new_all(),
            disks: sysinfo::Disks::new_with_refreshed_list(),
            networks: sysinfo::Networks::new_with_refreshed_list(),
            last: Instant::now(),
        }
    }
}

impl LocalSampler {
    /// 采样一轮(调用方控制频率,建议 ≥1s,保证 CPU/网络增量准确)
    pub fn sample(&mut self) -> SystemMetrics {
        let dt = self.last.elapsed().as_secs_f64().max(0.01);
        self.last = Instant::now();

        self.sys
            .refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        self.sys.refresh_memory();
        self.sys.refresh_cpu_usage();
        self.disks.refresh(false);
        self.networks.refresh(false);

        let load = sysinfo::System::load_average();
        let mut m = SystemMetrics {
            cpu_percent: self.sys.global_cpu_usage(),
            mem_total_kb: self.sys.total_memory() / 1024,
            mem_used_kb: self.sys.used_memory() / 1024,
            swap_total_kb: self.sys.total_swap() / 1024,
            swap_used_kb: self.sys.used_swap() / 1024,
            load: [load.one as f32, load.five as f32, load.fifteen as f32],
            uptime_secs: sysinfo::System::uptime(),
            rtt_ms: 0,
            os_pretty: sysinfo::System::long_os_version().unwrap_or_default(),
            kernel_name: sysinfo::System::name().unwrap_or_default(),
            kernel_release: sysinfo::System::kernel_version().unwrap_or_default(),
            arch: sysinfo::System::cpu_arch(),
            hostname: sysinfo::System::host_name().unwrap_or_default(),
            cpu_cores: self.sys.cpus().len() as u32,
            cpu_model: self
                .sys
                .cpus()
                .first()
                .map(|c| c.brand().to_string())
                .unwrap_or_default(),
            cpu_mhz: self
                .sys
                .cpus()
                .first()
                .map(|c| c.frequency() as f64)
                .unwrap_or(0.0),
            // 缓存/BogoMips/CPU 分项 sysinfo 拿不到,详细页显示 —
            ..Default::default()
        };

        // 全量进程,不排序不截断:侧栏支持按内存/CPU 切换排序,在那里取前 10
        m.procs = self
            .sys
            .processes()
            .values()
            .map(|p| ProcInfo {
                rss_kb: p.memory() / 1024,
                cpu: p.cpu_usage(),
                command: p.name().to_string_lossy().into_owned(),
            })
            .collect();

        let mut rx = 0u64;
        let mut tx = 0u64;
        for (name, data) in self.networks.iter() {
            let n = name.to_lowercase();
            let virt = n == "lo0" || n == "lo" || n.starts_with("utun") || n.starts_with("awdl");
            if !virt {
                rx += data.received();
                tx += data.transmitted();
            }
            // 详细页的每网卡表:全量(含 lo),received()/transmitted() 是本轮增量
            m.nets.push(NetIfInfo {
                name: name.clone(),
                rx_bytes: data.total_received(),
                tx_bytes: data.total_transmitted(),
                rx_bps: data.received() as f64 / dt,
                tx_bps: data.transmitted() as f64 / dt,
            });
        }
        m.net_rx_bps = rx as f64 / dt;
        m.net_tx_bps = tx as f64 / dt;

        m.disks = self
            .disks
            .iter()
            .filter(|d| {
                let mp = d.mount_point().to_string_lossy();
                mp == "/" || !mp.starts_with("/System/") && !mp.starts_with("/dev/")
            })
            .map(|d| DiskInfo {
                fs: d.file_system().to_string_lossy().into_owned(),
                path: d.mount_point().to_string_lossy().into_owned(),
                used_kb: (d.total_space() - d.available_space()) / 1024,
                avail_kb: d.available_space() / 1024,
                total_kb: d.total_space() / 1024,
            })
            .collect();
        m
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"\
=STAT
cpu  4705217 3560 584321 369291123 16420 0 4230 0 0 0
=LOAD
0.01 0.04 0.00 1/234 5678
=UP
35873212.41 71746148.10
=MEM
MemTotal:        1767468 kB
MemFree:          101344 kB
MemAvailable:    1377468 kB
SwapTotal:             0 kB
SwapFree:              0 kB
=NET
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  123456   100    0    0    0     0          0         0   123456   100    0    0    0     0       0          0
  eth0: 9876543210 100000 0    0    0     0          0         0  123456789  90000    0    0    0     0       0          0
=PS
    RSS %CPU COMMAND
  44608  2.0 AliYunDunMonito
  15584  0.7 AliYunDun
      0  0.3 rcu_preempt
=DF
Filesystem     1024-blocks     Used Available Capacity Mounted on
udev              8196000        0   8196000   0% /dev
/dev/vda1        41152832 10381920  29069488  27% /
tmpfs             8196000        0   8196000   0% /dev/shm
overlay        41152832 10381920  29069488  27% /var/lib/docker
=OS
PRETTY_NAME="Ubuntu 24.04.4 LTS"
K=Linux
R=6.8.0-136-generic
M=x86_64
H=servercenter
=CPUINFO
Architecture:                    x86_64
CPU(s):                          16
Thread(s) per core:              2
Model name:                      Intel(R) Core(TM) i9-7960X CPU @ 2.80GHz
CPU MHz:                         1200.000
L3 cache:                        22528 KB
BogoMIPS:                        5599.85
"#;

    #[test]
    fn parse_stat() {
        let v = parse_proc_stat("cpu  4705 356 584 369291 1642 19 423 7 0 0").unwrap();
        assert_eq!(v, [4705, 356, 584, 369291, 1642, 19, 423, 7]);
    }

    #[test]
    fn parse_load() {
        assert_eq!(
            parse_loadavg("0.01 0.04 0.00 1/234 5678"),
            Some([0.01, 0.04, 0.0])
        );
    }

    #[test]
    fn parse_net() {
        assert_eq!(
            parse_net_dev_line("  eth0: 9876543210 100000 0 0 0 0 0 0 123456789 90000 0 0 0 0 0 0"),
            Some(("eth0".to_string(), 9876543210, 123456789))
        );
        // lo 也解析(详细页要显示),聚合速率才排除
        assert_eq!(
            parse_net_dev_line("    lo: 1 2 3 4 5 6 7 8 9 10"),
            Some(("lo".to_string(), 1, 9))
        );
    }

    #[test]
    fn parse_ps() {
        assert!(parse_ps_line("    RSS %CPU COMMAND").is_none());
        let p = parse_ps_line("  44608  2.0 AliYunDunMonito").unwrap();
        assert_eq!(p.rss_kb, 44608);
        assert!((p.cpu - 2.0).abs() < 0.01);
        assert_eq!(p.command, "AliYunDunMonito");
    }

    #[test]
    fn parse_df() {
        assert!(
            parse_df_line("Filesystem     1024-blocks     Used Available Capacity Mounted on")
                .is_none()
        );
        // 虚拟文件系统也解析出来(详细页显示),由 is_real 给显示层过滤
        let t = parse_df_line("tmpfs  8196000 0 8196000 0% /dev/shm").unwrap();
        assert!(!t.is_real());
        let o = parse_df_line("overlay  41152832 10381920 29069488 27% /var/lib/docker").unwrap();
        assert!(!o.is_real());
        let d = parse_df_line("/dev/vda1  41152832 10381920  29069488  27% /").unwrap();
        assert!(d.is_real());
        assert_eq!(d.fs, "/dev/vda1");
        assert_eq!(d.path, "/");
        assert_eq!(d.total_kb, 41152832);
        assert_eq!(d.used_kb, 10381920);
        assert_eq!(d.avail_kb, 29069488);
    }

    #[test]
    fn parse_lscpu_size() {
        assert_eq!(parse_size_kb("22528 KB"), 22528);
        assert_eq!(parse_size_kb("22.5 MiB"), 23040);
        assert_eq!(parse_size_kb("1G"), 1024 * 1024);
        assert_eq!(parse_size_kb(""), 0);
    }

    #[test]
    fn numeric_cpu_model_does_not_override_model_name() {
        let mut m = SystemMetrics::default();
        parse_lscpu_line("Model name: Intel(R) Xeon(R) Platinum 8475B", &mut m);
        parse_lscpu_line("Model: 143", &mut m);
        assert_eq!(m.cpu_model, "Intel(R) Xeon(R) Platinum 8475B");
    }

    #[test]
    fn full_output() {
        let mut st = MonitorState::default();
        let m = st.update(SAMPLE, 12);
        assert_eq!(m.rtt_ms, 12);
        assert_eq!(m.mem_total_kb, 1767468);
        assert_eq!(m.mem_used_kb, 1767468 - 1377468);
        assert_eq!(m.load, [0.01, 0.04, 0.0]);
        assert_eq!(m.uptime_secs, 35873212);
        assert_eq!(m.procs.len(), 3);
        assert_eq!(m.procs[0].command, "AliYunDunMonito");
        // df 全量保留(udev/vda1/tmpfs/overlay),显示层过滤
        assert_eq!(m.disks.len(), 4);
        assert_eq!(m.disks.iter().filter(|d| d.is_real()).count(), 1);
        // OS / CPUINFO
        assert_eq!(m.os_pretty, "Ubuntu 24.04.4 LTS");
        assert_eq!(m.kernel_name, "Linux");
        assert_eq!(m.kernel_release, "6.8.0-136-generic");
        assert_eq!(m.arch, "x86_64");
        assert_eq!(m.hostname, "servercenter");
        assert_eq!(m.cpu_model, "Intel(R) Core(TM) i9-7960X CPU @ 2.80GHz");
        assert_eq!(m.cpu_cores, 16);
        assert_eq!(m.cpu_mhz, 1200.0);
        assert_eq!(m.cpu_cache_kb, 22528);
        assert_eq!(m.cpu_bogomips, 5599.85);
        // 每网卡(含 lo)
        assert_eq!(m.nets.len(), 2);
        assert_eq!(m.nets[0].name, "lo");
        // 第一轮无增量参照
        assert_eq!(m.net_rx_bps, 0.0);
        assert_eq!(m.cpu_percent, 0.0);
        assert!(m.cpu_busy.is_none());
        // 第二轮:user 计数 +100,其余不变 → 增量全在 user 上
        let sample2 = SAMPLE.replacen("cpu  4705217", "cpu  4705317", 1);
        let m2 = st.update(&sample2, 10);
        assert!(m2.cpu_percent >= 0.0 && m2.cpu_percent <= 100.0);
        let busy = m2.cpu_busy.expect("第二轮应有 CPU 分项");
        assert!((busy.user - 100.0).abs() < 0.01);
        assert!(busy.idle.abs() < 0.01 && busy.system.abs() < 0.01);
    }
}
