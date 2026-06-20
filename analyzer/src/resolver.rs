//! Resolves runtime return addresses into `function (file:line)` using DWARF/
//! symbol info from every file-backed mapping (target + libc + libstdc++ + ...).
//! Per-file loaders/load-bases built lazily; ASLR-correct and survives target
//! exit. See DESIGN.md.

use object::{Object, ObjectSegment};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct Region {
    start: u64,
    end: u64,
    path: PathBuf,
}

struct FileState {
    lowest_start: u64,
    tried: bool,
    loader: Option<Rc<addr2line::Loader>>,
    load_base: u64,
}

pub struct Resolver {
    regions: Vec<Region>,
    files: RefCell<HashMap<PathBuf, FileState>>,
}

impl Resolver {
    pub fn new(_binary: &Path, pid: u32) -> io::Result<Self> {
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
        let mut regions = Vec::new();
        let mut files: HashMap<PathBuf, FileState> = HashMap::new();

        for line in maps.lines() {
            let mut fields = line.split_whitespace();
            let Some(range) = fields.next() else { continue };
            for _ in 0..4 {
                fields.next();
            }
            let Some(path_field) = fields.next() else {
                continue;
            };
            if !path_field.starts_with('/') || line.ends_with("(deleted)") {
                continue;
            }
            let Some((start, end)) = parse_range(range) else {
                continue;
            };
            let path = PathBuf::from(path_field);
            regions.push(Region {
                start,
                end,
                path: path.clone(),
            });
            files
                .entry(path)
                .and_modify(|f| f.lowest_start = f.lowest_start.min(start))
                .or_insert(FileState {
                    lowest_start: start,
                    tried: false,
                    loader: None,
                    load_base: 0,
                });
        }

        regions.sort_by_key(|r| r.start);

        Ok(Self {
            regions,
            files: RefCell::new(files),
        })
    }

    pub fn resolve(&self, runtime_addr: usize) -> Vec<String> {
        let addr = runtime_addr as u64;
        let Some(path) = self.path_for(addr) else {
            return Vec::new();
        };

        let mut files = self.files.borrow_mut();
        let Some(state) = files.get_mut(&path) else {
            return Vec::new();
        };

        if !state.tried {
            state.tried = true;
            if let Ok(base) = compute_load_base(&path, state.lowest_start)
                && let Ok(loader) = addr2line::Loader::new(&path)
            {
                state.load_base = base;
                state.loader = Some(Rc::new(loader));
            }
        }

        let Some(loader) = state.loader.clone() else {
            return Vec::new();
        };
        let load_base = state.load_base;
        drop(files);

        let Some(static_addr) = addr.checked_sub(load_base) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        if let Ok(mut frames) = loader.find_frames(static_addr) {
            while let Ok(Some(frame)) = frames.next() {
                let name = frame
                    .function
                    .as_ref()
                    .and_then(|f| f.demangle().ok())
                    .map(|s| s.into_owned());
                let location = frame.location.as_ref().and_then(|loc| {
                    let line = loc.line?;
                    Some(format!("{}:{line}", loc.file.unwrap_or("?")))
                });
                let label = match (name, location) {
                    (Some(name), Some(loc)) => Some(format!("{name} ({loc})")),
                    (Some(name), None) => Some(name),
                    (None, Some(loc)) => Some(loc),
                    (None, None) => None,
                };
                if let Some(label) = label {
                    out.push(label);
                }
            }
        }
        if out.is_empty()
            && let Some(sym) = loader.find_symbol(static_addr)
        {
            out.push(sym.to_string());
        }
        out
    }

    fn path_for(&self, addr: u64) -> Option<PathBuf> {
        let idx = self
            .regions
            .partition_point(|r| r.start <= addr)
            .checked_sub(1)?;
        let region = &self.regions[idx];
        if addr >= region.start && addr < region.end {
            Some(region.path.clone())
        } else {
            None
        }
    }
}

fn parse_range(range: &str) -> Option<(u64, u64)> {
    let (start, end) = range.split_once('-')?;
    Some((
        u64::from_str_radix(start, 16).ok()?,
        u64::from_str_radix(end, 16).ok()?,
    ))
}

fn compute_load_base(path: &Path, lowest_start: u64) -> io::Result<u64> {
    let data = std::fs::read(path)?;
    let obj = object::File::parse(&*data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let min_vaddr = obj.segments().map(|s| s.address()).min().unwrap_or(0);
    Ok(lowest_start.saturating_sub(min_vaddr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_range_handles_hex_bounds() {
        assert_eq!(parse_range("5500-5600"), Some((0x5500, 0x5600)));
        assert_eq!(parse_range("garbage"), None);
    }

    #[test]
    fn path_for_dispatches_to_the_containing_region() {
        let resolver = Resolver {
            regions: vec![
                Region {
                    start: 0x1000,
                    end: 0x2000,
                    path: PathBuf::from("/a"),
                },
                Region {
                    start: 0x3000,
                    end: 0x4000,
                    path: PathBuf::from("/b"),
                },
            ],
            files: RefCell::new(HashMap::new()),
        };

        assert_eq!(resolver.path_for(0x1500), Some(PathBuf::from("/a")));
        assert_eq!(resolver.path_for(0x3000), Some(PathBuf::from("/b")));
        assert_eq!(resolver.path_for(0x3fff), Some(PathBuf::from("/b")));
        assert_eq!(resolver.path_for(0x2500), None);
        assert_eq!(resolver.path_for(0x9000), None);
        assert_eq!(resolver.path_for(0x0500), None);
    }

    #[test]
    fn resolve_self_finds_a_known_function() {
        let exe = std::env::current_exe().expect("current exe");
        let pid = std::process::id();
        let resolver = Resolver::new(&exe, pid).expect("build resolver");

        let addr = resolve_self_finds_a_known_function as *const () as usize;
        let frames = resolver.resolve(addr);
        assert!(
            frames.iter().any(|f| f.contains("resolve_self_finds_a_known_function")),
            "expected to resolve this function, got {frames:?}"
        );
    }
}
