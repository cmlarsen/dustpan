use chrono::{DateTime, Utc};
use jwalk::rayon::{ThreadPool, ThreadPoolBuilder};
use jwalk::{Parallelism, WalkDirGeneric};
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Default, Clone, Debug)]
pub struct DirStats {
    pub bytes: u64,
    pub exclusive: u64,
    pub files: u64,
    pub newest: Option<DateTime<Utc>>,
    pub breakdown: HashMap<String, u64>,
}

#[derive(Default)]
pub struct Opts<'a> {
    pub breakdown_under: Option<&'a Path>,
    pub mtime_ignore: &'a [&'a str],
}

#[derive(Default, Debug, Clone, Copy)]
struct Stat {
    bytes: u64,
    nlink: u64,
    ino: u64,
    dev: u64,
    mtime: i64,
    is_dir: bool,
}

impl Stat {
    fn of(m: &std::fs::Metadata) -> Self {
        Stat {
            bytes: m.blocks() * 512,
            nlink: m.nlink(),
            ino: m.ino(),
            dev: m.dev(),
            mtime: m.mtime(),
            is_dir: m.is_dir(),
        }
    }
}

const MAX_CONCURRENT_WALKS: usize = 6;

fn pool() -> Arc<ThreadPool> {
    static POOL: OnceLock<Arc<ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(8, |n| n.get() * 2);
        Arc::new(
            ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("dustpan-walk-{i}"))
                .build()
                .expect("walk thread pool"),
        )
    })
    .clone()
}

struct WalkPermit;

static WALKS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

impl WalkPermit {
    fn acquire() -> Self {
        let (lock, cvar) = &WALKS;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= MAX_CONCURRENT_WALKS {
            n = cvar.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        WalkPermit
    }
}

impl Drop for WalkPermit {
    fn drop(&mut self) {
        let (lock, cvar) = &WALKS;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        *n -= 1;
        cvar.notify_one();
    }
}

fn to_time(secs: i64) -> Option<DateTime<Utc>> {
    let secs = u64::try_from(secs).ok()?;
    Some((UNIX_EPOCH + Duration::from_secs(secs)).into())
}

pub fn dir_stats(root: &Path) -> DirStats {
    dir_stats_opts(root, &Opts::default())
}

pub fn dir_stats_opts(root: &Path, opts: &Opts) -> DirStats {
    let mut stats = DirStats::default();
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        return stats;
    };
    let root_stat = Stat::of(&meta);
    stats.newest = to_time(root_stat.mtime);
    if !meta.is_dir() {
        stats.bytes = root_stat.bytes;
        stats.exclusive = if root_stat.nlink > 1 { 0 } else { root_stat.bytes };
        stats.files = 1;
        return stats;
    }

    let _permit = WalkPermit::acquire();
    let walk = WalkDirGeneric::<((), Option<Stat>)>::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(Parallelism::RayonExistingPool {
            pool: pool(),
            busy_timeout: None,
        })
        .process_read_dir(|_, _, _, children| {
            for child in children.iter_mut().flatten() {
                if let Ok(m) = child.metadata() {
                    child.client_state = Some(Stat::of(&m));
                }
            }
        });

    let mut links: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
    let mut newest = root_stat.mtime;
    for entry in walk.into_iter().flatten() {
        let Some(st) = entry.client_state else { continue };
        if entry.depth == 0 {
            continue;
        }
        let path = entry.path();
        if st.mtime > newest
            && (opts.mtime_ignore.is_empty() || !ignored(&path, root, opts.mtime_ignore))
        {
            newest = st.mtime;
        }
        let bucket = opts.breakdown_under.and_then(|under| {
            path.strip_prefix(under)
                .ok()
                .and_then(|rel| rel.components().next())
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
        });
        let counted = if st.is_dir || st.nlink <= 1 {
            stats.exclusive += st.bytes;
            true
        } else {
            let e = links.entry((st.dev, st.ino)).or_insert((0, st.nlink, st.bytes));
            e.0 += 1;
            e.0 == 1
        };
        if !st.is_dir {
            stats.files += 1;
        }
        if counted {
            stats.bytes += st.bytes;
            if let Some(b) = bucket {
                *stats.breakdown.entry(b).or_default() += st.bytes;
            }
        }
    }
    for (seen, nlink, bytes) in links.into_values() {
        if seen >= nlink {
            stats.exclusive += bytes;
        }
    }
    stats.newest = to_time(newest);
    stats
}

fn ignored(path: &Path, root: &Path, names: &[&str]) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .any(|c| names.iter().any(|n| c.as_os_str() == *n))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn blocks(p: &Path) -> u64 {
        fs::metadata(p).unwrap().blocks() * 512
    }

    #[test]
    fn counts_internal_hardlinks_once_and_external_ones_as_shared() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), vec![1u8; 10_000]).unwrap();
        fs::hard_link(root.join("a"), root.join("a2")).unwrap();
        fs::write(root.join("shared"), vec![2u8; 20_000]).unwrap();
        fs::hard_link(root.join("shared"), outside.path().join("shared")).unwrap();
        fs::write(root.join("solo"), vec![3u8; 5_000]).unwrap();

        let s = dir_stats(&root);
        let a = blocks(&root.join("a"));
        let shared = blocks(&root.join("shared"));
        let solo = blocks(&root.join("solo"));
        assert_eq!(s.files, 4);
        assert_eq!(s.bytes, a + shared + solo);
        assert_eq!(s.exclusive, a + solo);
    }

    #[test]
    fn breakdown_and_mtime_ignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("apps/big")).unwrap();
        fs::create_dir_all(root.join("apps/small")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("apps/big/f"), vec![0u8; 50_000]).unwrap();
        fs::write(root.join("apps/small/f"), vec![0u8; 1_000]).unwrap();
        fs::write(root.join("node_modules/x"), b"x").unwrap();

        let old = std::time::SystemTime::now() - Duration::from_secs(86_400 * 10);
        for p in ["apps/big/f", "apps/small/f", "apps/big", "apps/small", "apps", ""] {
            let f = fs::File::open(root.join(p)).unwrap();
            f.set_modified(old).unwrap();
        }
        fs::File::open(root)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let under = root.join("apps");
        let s = dir_stats_opts(
            root,
            &Opts {
                breakdown_under: Some(&under),
                mtime_ignore: &["node_modules"],
            },
        );
        assert!(s.breakdown["big"] >= 50_000);
        assert!(s.breakdown["small"] < s.breakdown["big"]);
        let age = (Utc::now() - s.newest.unwrap()).num_days();
        assert!(age >= 9, "node_modules should not count toward newest, age={age}");
    }

    #[test]
    fn missing_path_is_empty() {
        let s = dir_stats(Path::new("/definitely/not/here"));
        assert_eq!(s.bytes, 0);
        assert!(s.newest.is_none());
    }
}
