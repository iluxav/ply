//! A 9P2000.L file server over one host directory: how a `ply.dev.toml`
//! link — the source tree a developer edits on the Mac — reaches the guest
//! live, over virtio-9p.
//!
//! The wire protocol is 9P2000.L as Linux's `fs/9p` speaks it: every message
//! is `size[4] type[1] tag[2] body`, little-endian, strings are `len[2]`
//! then bytes, and errors are `Rlerror` carrying a **Linux** errno — the
//! guest is Linux whatever the host is, so a macOS `ENOTEMPTY` (66) must
//! cross as a Linux one (39), which `errno_for` does by error *kind*.
//!
//! # Portable on purpose
//!
//! Nothing here touches Hypervisor.framework: this is bytes in, bytes out,
//! over `std::fs`. The tests drive it exactly the way the guest driver does
//! and run on Linux CI, where the rest of the VM backend cannot. The
//! virtio-mmio transport that feeds it is `p9dev`, and is macOS-only.
//!
//! # What it promises, and what it does not
//!
//! * Every file appears owned by the app's user (`uid`/`gid`), whatever the
//!   host says: the share is the developer's own tree, and a `[package]
//!   user` app that could not read its own source would be the surprise,
//!   not the security.
//! * No caching, no leases, no locks: the guest mounts with `cache=none` so
//!   an edit on the host is what the next read sees, `Tlock` always
//!   succeeds and `Tgetlock` always finds the range free (the same answer a
//!   single-writer developer machine gives anyway).
//! * Extended attributes and device nodes are refused with `EOPNOTSUPP`.
//! * A walk cannot leave the root: `..` at the top stays at the top.

use std::collections::HashMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The one version this server speaks. A client asking for anything else is
/// answered `unknown`, which is 9P's way of saying "not this".
pub const VERSION: &str = "9P2000.L";

/// The most a message may be. The guest asks for `msize` on `Tversion` and
/// gets the smaller of its wish and this; 256 KiB moves a source tree fast
/// enough and bounds what one request can make the host allocate.
pub const MAX_MSIZE: u32 = 256 * 1024;

/// The fixed header of every message.
const HEADER: u32 = 7;
/// `Rread`/`Rreaddir` overhead beyond the header: `count[4]`.
const IO_HEADER: u32 = HEADER + 4;

// Message types, from `include/net/9p/9p.h`.
const TLERROR: u8 = 6;
const RLERROR: u8 = 7;
const TSTATFS: u8 = 8;
const TLOPEN: u8 = 12;
const TLCREATE: u8 = 14;
const TSYMLINK: u8 = 16;
const TMKNOD: u8 = 18;
const TRENAME: u8 = 20;
const TREADLINK: u8 = 22;
const TGETATTR: u8 = 24;
const TSETATTR: u8 = 26;
const TXATTRWALK: u8 = 30;
const TXATTRCREATE: u8 = 32;
const TREADDIR: u8 = 40;
const TFSYNC: u8 = 50;
const TLOCK: u8 = 52;
const TGETLOCK: u8 = 54;
const TLINK: u8 = 70;
const TMKDIR: u8 = 72;
const TRENAMEAT: u8 = 74;
const TUNLINKAT: u8 = 76;
const TVERSION: u8 = 100;
const TAUTH: u8 = 102;
const TATTACH: u8 = 104;
const TFLUSH: u8 = 108;
const TWALK: u8 = 110;
const TREAD: u8 = 116;
const TWRITE: u8 = 118;
const TCLUNK: u8 = 120;
const TREMOVE: u8 = 122;

// Linux errno values: the guest's, never the host's.
const EPERM: u32 = 1;
const ENOENT: u32 = 2;
const EIO: u32 = 5;
const EBADF: u32 = 9;
const EACCES: u32 = 13;
const EEXIST: u32 = 17;
const ENOTDIR: u32 = 20;
const EISDIR: u32 = 21;
const EINVAL: u32 = 22;
const ENOSPC: u32 = 28;
const EROFS: u32 = 30;
const ENAMETOOLONG: u32 = 36;
const ENOTEMPTY: u32 = 39;
const ELOOP: u32 = 40;
const EOPNOTSUPP: u32 = 95;

// Qid type bits.
const QTDIR: u8 = 0x80;
const QTSYMLINK: u8 = 0x02;
const QTFILE: u8 = 0x00;

// `d_type` values in `Rreaddir`.
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

// Linux open(2) flags as the guest sends them (asm-generic values, which
// arm64 uses).
const O_ACCMODE: u32 = 0o3;
const O_WRONLY: u32 = 0o1;
const O_RDWR: u32 = 0o2;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;

// `Tsetattr` valid bits.
const SETATTR_MODE: u32 = 1 << 0;
const SETATTR_SIZE: u32 = 1 << 3;
const SETATTR_ATIME: u32 = 1 << 4;
const SETATTR_MTIME: u32 = 1 << 5;
const SETATTR_ATIME_SET: u32 = 1 << 7;
const SETATTR_MTIME_SET: u32 = 1 << 8;

/// `Rgetattr`'s `valid`: everything up to and including `btime` is
/// answered, `gen` and `data_version` are not.
const GETATTR_BASIC: u64 = 0x0000_07ff;

/// `AT_REMOVEDIR` in `Tunlinkat`'s flags.
const AT_REMOVEDIR: u32 = 0x200;

/// The unique identity of a file as 9P names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Qid {
    pub kind: u8,
    pub version: u32,
    pub path: u64,
}

impl Qid {
    fn of(md: &Metadata) -> Qid {
        let ft = md.file_type();
        let kind = if ft.is_dir() {
            QTDIR
        } else if ft.is_symlink() {
            QTSYMLINK
        } else {
            QTFILE
        };
        Qid {
            kind,
            // The version is what a client with a cache would compare; the
            // guest runs without one, and the mtime is the honest answer.
            version: md.mtime() as u32,
            path: md.ino(),
        }
    }
}

/// One open handle behind a fid.
enum Open {
    File(File),
    /// A directory, snapshotted at open: 9P reads it by offset, and a
    /// listing that changed under the guest between two `Treaddir`s would
    /// otherwise repeat or skip entries.
    Dir(Vec<DirEnt>),
}

struct DirEnt {
    name: String,
    qid: Qid,
    kind: u8,
}

struct Fid {
    /// Host path, always under the root.
    path: PathBuf,
    open: Option<Open>,
}

/// The server: one shared directory, one client (the guest's kernel), many
/// fids.
pub struct Server {
    root: PathBuf,
    uid: u32,
    gid: u32,
    msize: u32,
    fids: HashMap<u32, Fid>,
}

/// A request the server refused, with the Linux errno the guest gets.
#[derive(Debug)]
struct Refused(u32);

type Reply = Result<Vec<u8>, Refused>;

impl Server {
    /// Serve `root`, reporting every file as owned by `uid:gid`.
    ///
    /// The root is canonicalised once so that a walk can compare paths
    /// against it; a root that does not exist is refused here, before any
    /// guest could mount it.
    pub fn new(root: &Path, uid: u32, gid: u32) -> io::Result<Server> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(Server {
            root,
            uid,
            gid,
            msize: MAX_MSIZE,
            fids: HashMap::new(),
        })
    }

    /// The negotiated message size — what the transport must be able to
    /// carry in one request or reply.
    pub fn msize(&self) -> u32 {
        self.msize
    }

    /// Answer one complete request message with one complete reply message,
    /// both with their `size[4] type[1] tag[2]` headers.
    ///
    /// Never panics and never fails: a request the server cannot parse is
    /// answered `Rlerror EINVAL` under its own tag, and a request too short
    /// to carry a tag gets one under `NOTAG` (0xffff) — the guest driver
    /// drops a reply it has no request for, which is the right outcome for
    /// bytes that were never a request.
    pub fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        let mut r = Reader::new(request);
        let (Some(_size), Some(kind), Some(tag)) = (r.u32(), r.u8(), r.u16()) else {
            return message(RLERROR, 0xffff, &u32_bytes(EINVAL));
        };
        match self.dispatch(kind, &mut r) {
            Ok(body) => message(kind + 1, tag, &body),
            Err(Refused(errno)) => message(RLERROR, tag, &u32_bytes(errno)),
        }
    }

    fn dispatch(&mut self, kind: u8, r: &mut Reader<'_>) -> Reply {
        match kind {
            TVERSION => self.version(r),
            TATTACH => self.attach(r),
            TWALK => self.walk(r),
            TLOPEN => self.lopen(r),
            TLCREATE => self.lcreate(r),
            TGETATTR => self.getattr(r),
            TSETATTR => self.setattr(r),
            TREADDIR => self.readdir(r),
            TREAD => self.read(r),
            TWRITE => self.write(r),
            TCLUNK => self.clunk(r),
            TREMOVE => self.remove(r),
            TFSYNC => self.fsync(r),
            TMKDIR => self.mkdir(r),
            TSYMLINK => self.symlink(r),
            TREADLINK => self.readlink(r),
            TLINK => self.link(r),
            TRENAME => self.rename(r),
            TRENAMEAT => self.renameat(r),
            TUNLINKAT => self.unlinkat(r),
            TSTATFS => self.statfs(r),
            TLOCK => Ok(vec![0]), // P9_LOCK_SUCCESS: nobody else holds anything
            TGETLOCK => self.getlock(r),
            TFLUSH => Ok(Vec::new()), // every request is answered before the next is read
            TXATTRWALK | TXATTRCREATE | TMKNOD | TAUTH => Err(Refused(EOPNOTSUPP)),
            // `Tlerror` is never sent, and `Rlerror + 1` is not a type.
            TLERROR => Err(Refused(EINVAL)),
            _ => Err(Refused(EOPNOTSUPP)),
        }
    }

    // ------------------------------------------------------------ session

    fn version(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(msize), Some(version)) = (r.u32(), r.string()) else {
            return Err(Refused(EINVAL));
        };
        // A new session: every fid the old one held is gone.
        self.fids.clear();
        self.msize = msize.min(MAX_MSIZE);
        let answer = if version == VERSION {
            VERSION
        } else {
            "unknown"
        };
        let mut w = Writer::new();
        w.u32(self.msize);
        w.string(answer);
        Ok(w.done())
    }

    fn attach(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(_afid), Some(_uname), Some(_aname), Some(_n_uname)) =
            (r.u32(), r.u32(), r.string(), r.string(), r.u32())
        else {
            return Err(Refused(EINVAL));
        };
        let md = lstat(&self.root)?;
        let qid = Qid::of(&md);
        self.fids.insert(
            fid,
            Fid {
                path: self.root.clone(),
                open: None,
            },
        );
        Ok(qid_bytes(qid))
    }

    fn walk(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(newfid), Some(count)) = (r.u32(), r.u32(), r.u16()) else {
            return Err(Refused(EINVAL));
        };
        let mut names = Vec::with_capacity(count as usize);
        for _ in 0..count {
            names.push(r.string().ok_or(Refused(EINVAL))?);
        }
        let start = self.fid(fid)?.path.clone();
        let mut path = start;
        let mut qids = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            let next = self.step(&path, name);
            match lstat(&next) {
                Ok(md) => {
                    qids.push(Qid::of(&md));
                    path = next;
                }
                Err(e) => {
                    // The first step failing is an error; a later one is a
                    // partial walk, which the client reads as "that far and
                    // no further" and never binds `newfid`.
                    if i == 0 {
                        return Err(e);
                    }
                    break;
                }
            }
        }
        if qids.len() == names.len() {
            self.fids.insert(newfid, Fid { path, open: None });
        }
        let mut w = Writer::new();
        w.u16(qids.len() as u16);
        for qid in qids {
            w.qid(qid);
        }
        Ok(w.done())
    }

    /// One path component, with `..` clamped at the root and `.` a no-op.
    /// Anything with a slash or a NUL is a name the guest could not have
    /// produced from a real path, and is treated as `.` rather than opened.
    fn step(&self, from: &Path, name: &str) -> PathBuf {
        match name {
            "" | "." => from.to_path_buf(),
            ".." => {
                if from == self.root {
                    self.root.clone()
                } else {
                    from.parent().unwrap_or(&self.root).to_path_buf()
                }
            }
            n if n.contains('/') || n.contains('\0') => from.to_path_buf(),
            n => from.join(n),
        }
    }

    fn clunk(&mut self, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().ok_or(Refused(EINVAL))?;
        self.fids.remove(&fid);
        Ok(Vec::new())
    }

    // ------------------------------------------------------------- files

    fn lopen(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(flags)) = (r.u32(), r.u32()) else {
            return Err(Refused(EINVAL));
        };
        let path = self.fid(fid)?.path.clone();
        let md = lstat(&path)?;
        let qid = Qid::of(&md);
        let open = if md.is_dir() {
            Open::Dir(self.list(&path)?)
        } else {
            Open::File(open_flags(flags).open(&path).map_err(refuse)?)
        };
        self.fid_mut(fid)?.open = Some(open);
        let mut w = Writer::new();
        w.qid(qid);
        w.u32(0); // iounit: the client sizes I/O from msize
        Ok(w.done())
    }

    fn lcreate(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(name), Some(flags), Some(mode), Some(_gid)) =
            (r.u32(), r.string(), r.u32(), r.u32(), r.u32())
        else {
            return Err(Refused(EINVAL));
        };
        let dir = self.fid(fid)?.path.clone();
        let path = self.child(&dir, &name)?;
        let file = open_flags(flags)
            .create_new(true)
            .mode(mode & 0o7777)
            .open(&path)
            .map_err(refuse)?;
        let qid = Qid::of(&file.metadata().map_err(refuse)?);
        // The fid now names the new file, open.
        *self.fid_mut(fid)? = Fid {
            path,
            open: Some(Open::File(file)),
        };
        let mut w = Writer::new();
        w.qid(qid);
        w.u32(0);
        Ok(w.done())
    }

    fn read(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(offset), Some(count)) = (r.u32(), r.u64(), r.u32()) else {
            return Err(Refused(EINVAL));
        };
        let count = count.min(self.msize.saturating_sub(IO_HEADER)) as usize;
        let file = match &self.fid(fid)?.open {
            Some(Open::File(f)) => f,
            Some(Open::Dir(_)) => return Err(Refused(EISDIR)),
            None => return Err(Refused(EBADF)),
        };
        let mut buf = vec![0u8; count];
        // A short read is not an error: it is how the end of a file looks.
        let mut got = 0;
        while got < count {
            match file.read_at(&mut buf[got..], offset + got as u64) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(refuse(e)),
            }
        }
        buf.truncate(got);
        let mut w = Writer::new();
        w.u32(got as u32);
        w.bytes(&buf);
        Ok(w.done())
    }

    fn write(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(offset), Some(count)) = (r.u32(), r.u64(), r.u32()) else {
            return Err(Refused(EINVAL));
        };
        let data = r.bytes(count as usize).ok_or(Refused(EINVAL))?;
        let file = match &self.fid(fid)?.open {
            Some(Open::File(f)) => f,
            Some(Open::Dir(_)) => return Err(Refused(EISDIR)),
            None => return Err(Refused(EBADF)),
        };
        file.write_all_at(data, offset).map_err(refuse)?;
        Ok(u32_bytes(count))
    }

    fn fsync(&mut self, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().ok_or(Refused(EINVAL))?;
        // `datasync[4]` follows in the Linux client's `Tfsync`; either way
        // the whole file is flushed — the difference is not worth a bug.
        if let Some(Open::File(f)) = &self.fid(fid)?.open {
            f.sync_all().map_err(refuse)?;
        }
        Ok(Vec::new())
    }

    // ------------------------------------------------------ attributes

    fn getattr(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(_mask)) = (r.u32(), r.u64()) else {
            return Err(Refused(EINVAL));
        };
        let path = self.fid(fid)?.path.clone();
        let md = lstat(&path)?;
        let mut w = Writer::new();
        w.u64(GETATTR_BASIC);
        w.qid(Qid::of(&md));
        w.u32(md.mode());
        w.u32(self.uid);
        w.u32(self.gid);
        w.u64(md.nlink());
        w.u64(0); // rdev: no device nodes here
        w.u64(md.len());
        w.u64(4096); // blksize
        w.u64(md.len().div_ceil(512));
        w.u64(md.atime() as u64);
        w.u64(md.atime_nsec() as u64);
        w.u64(md.mtime() as u64);
        w.u64(md.mtime_nsec() as u64);
        w.u64(md.ctime() as u64);
        w.u64(md.ctime_nsec() as u64);
        w.u64(0); // btime
        w.u64(0);
        w.u64(0); // gen
        w.u64(0); // data_version
        Ok(w.done())
    }

    fn setattr(&mut self, r: &mut Reader<'_>) -> Reply {
        let (
            Some(fid),
            Some(valid),
            Some(mode),
            Some(_uid),
            Some(_gid),
            Some(size),
            Some(atime_sec),
            Some(atime_nsec),
            Some(mtime_sec),
            Some(mtime_nsec),
        ) = (
            r.u32(),
            r.u32(),
            r.u32(),
            r.u32(),
            r.u32(),
            r.u64(),
            r.u64(),
            r.u64(),
            r.u64(),
            r.u64(),
        )
        else {
            return Err(Refused(EINVAL));
        };
        let path = self.fid(fid)?.path.clone();
        if valid & SETATTR_MODE != 0 {
            std::fs::set_permissions(&path, PermissionsExt::from_mode(mode & 0o7777))
                .map_err(refuse)?;
        }
        if valid & SETATTR_SIZE != 0 {
            match &self.fid(fid)?.open {
                Some(Open::File(f)) => f.set_len(size).map_err(refuse)?,
                _ => OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_len(size))
                    .map_err(refuse)?,
            }
        }
        if valid & (SETATTR_ATIME | SETATTR_MTIME) != 0 {
            let now = std::time::SystemTime::now();
            let stamp = |set: u32, sec: u64, nsec: u64| {
                if valid & set != 0 {
                    std::time::UNIX_EPOCH + std::time::Duration::new(sec, nsec as u32)
                } else {
                    now
                }
            };
            let mut times = std::fs::FileTimes::new();
            if valid & SETATTR_ATIME != 0 {
                times = times.set_accessed(stamp(SETATTR_ATIME_SET, atime_sec, atime_nsec));
            }
            if valid & SETATTR_MTIME != 0 {
                times = times.set_modified(stamp(SETATTR_MTIME_SET, mtime_sec, mtime_nsec));
            }
            // Best effort, like a `touch` on a file that is not ours to
            // date: the content is what the developer edits, not the stamp.
            if let Ok(f) = File::options().write(true).open(&path) {
                let _ = f.set_times(times);
            }
        }
        // uid/gid: ignored on purpose — every file is reported as the
        // app's, so there is nothing a chown could change.
        Ok(Vec::new())
    }

    // ------------------------------------------------------- directories

    fn list(&self, dir: &Path) -> Result<Vec<DirEnt>, Refused> {
        let mut out = Vec::new();
        let self_qid = Qid::of(&lstat(dir)?);
        let parent = if dir == self.root {
            self_qid
        } else {
            Qid::of(&lstat(dir.parent().unwrap_or(&self.root))?)
        };
        out.push(DirEnt {
            name: ".".into(),
            qid: self_qid,
            kind: DT_DIR,
        });
        out.push(DirEnt {
            name: "..".into(),
            qid: parent,
            kind: DT_DIR,
        });
        for entry in std::fs::read_dir(dir).map_err(refuse)? {
            let entry = entry.map_err(refuse)?;
            let Ok(md) = entry.metadata() else {
                continue; // vanished between the listing and the stat
            };
            let ft = md.file_type();
            let kind = if ft.is_dir() {
                DT_DIR
            } else if ft.is_symlink() {
                DT_LNK
            } else {
                DT_REG
            };
            out.push(DirEnt {
                name: entry.file_name().to_string_lossy().into_owned(),
                qid: Qid::of(&md),
                kind,
            });
        }
        Ok(out)
    }

    fn readdir(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(offset), Some(count)) = (r.u32(), r.u64(), r.u32()) else {
            return Err(Refused(EINVAL));
        };
        let count = count.min(self.msize.saturating_sub(IO_HEADER)) as usize;
        let entries = match &self.fid(fid)?.open {
            Some(Open::Dir(entries)) => entries,
            Some(Open::File(_)) => return Err(Refused(ENOTDIR)),
            None => return Err(Refused(EBADF)),
        };
        // The offset is opaque to the client and is simply "entries served
        // so far": entry `i` carries offset `i + 1`, and a request at
        // offset `n` resumes at entry `n`.
        let mut w = Writer::new();
        let mut used = 0usize;
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let size = 13 + 8 + 1 + 2 + entry.name.len();
            if used + size > count {
                break;
            }
            w.qid(entry.qid);
            w.u64(i as u64 + 1);
            w.u8(entry.kind);
            w.string(&entry.name);
            used += size;
        }
        let data = w.done();
        let mut out = Writer::new();
        out.u32(data.len() as u32);
        out.bytes(&data);
        Ok(out.done())
    }

    fn mkdir(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(dfid), Some(name), Some(mode), Some(_gid)) =
            (r.u32(), r.string(), r.u32(), r.u32())
        else {
            return Err(Refused(EINVAL));
        };
        let dir = self.fid(dfid)?.path.clone();
        let path = self.child(&dir, &name)?;
        std::fs::DirBuilder::new()
            .mode(mode & 0o7777)
            .create(&path)
            .map_err(refuse)?;
        Ok(qid_bytes(Qid::of(&lstat(&path)?)))
    }

    // ------------------------------------------------------------- links

    fn symlink(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(dfid), Some(name), Some(target), Some(_gid)) =
            (r.u32(), r.string(), r.string(), r.u32())
        else {
            return Err(Refused(EINVAL));
        };
        let dir = self.fid(dfid)?.path.clone();
        let path = self.child(&dir, &name)?;
        std::os::unix::fs::symlink(&target, &path).map_err(refuse)?;
        Ok(qid_bytes(Qid::of(&lstat(&path)?)))
    }

    fn readlink(&mut self, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().ok_or(Refused(EINVAL))?;
        let path = self.fid(fid)?.path.clone();
        let target = std::fs::read_link(&path).map_err(refuse)?;
        let mut w = Writer::new();
        w.string(&target.to_string_lossy());
        Ok(w.done())
    }

    fn link(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(dfid), Some(fid), Some(name)) = (r.u32(), r.u32(), r.string()) else {
            return Err(Refused(EINVAL));
        };
        let dir = self.fid(dfid)?.path.clone();
        let existing = self.fid(fid)?.path.clone();
        let path = self.child(&dir, &name)?;
        std::fs::hard_link(&existing, &path).map_err(refuse)?;
        Ok(Vec::new())
    }

    // ---------------------------------------------------- rename, remove

    fn rename(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(dfid), Some(name)) = (r.u32(), r.u32(), r.string()) else {
            return Err(Refused(EINVAL));
        };
        let from = self.fid(fid)?.path.clone();
        let dir = self.fid(dfid)?.path.clone();
        let to = self.child(&dir, &name)?;
        std::fs::rename(&from, &to).map_err(refuse)?;
        self.fid_mut(fid)?.path = to;
        Ok(Vec::new())
    }

    fn renameat(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(olddfid), Some(oldname), Some(newdfid), Some(newname)) =
            (r.u32(), r.string(), r.u32(), r.string())
        else {
            return Err(Refused(EINVAL));
        };
        let from = self.child(&self.fid(olddfid)?.path.clone(), &oldname)?;
        let to = self.child(&self.fid(newdfid)?.path.clone(), &newname)?;
        std::fs::rename(&from, &to).map_err(refuse)?;
        // A fid that named the moved file follows it, as it does on a real
        // filesystem where the fid is an inode, not a path.
        for f in self.fids.values_mut() {
            if f.path == from {
                f.path = to.clone();
            }
        }
        Ok(Vec::new())
    }

    fn unlinkat(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(dfid), Some(name), Some(flags)) = (r.u32(), r.string(), r.u32()) else {
            return Err(Refused(EINVAL));
        };
        let dir = self.fid(dfid)?.path.clone();
        let path = self.child(&dir, &name)?;
        if flags & AT_REMOVEDIR != 0 {
            std::fs::remove_dir(&path).map_err(refuse)?;
        } else {
            std::fs::remove_file(&path).map_err(refuse)?;
        }
        Ok(Vec::new())
    }

    fn remove(&mut self, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().ok_or(Refused(EINVAL))?;
        // The fid is clunked whether or not the remove succeeds — that is
        // the protocol, and it keeps a failed remove from leaking a handle.
        let Some(f) = self.fids.remove(&fid) else {
            return Err(Refused(EBADF));
        };
        if f.path == self.root {
            return Err(Refused(EPERM));
        }
        let md = lstat(&f.path)?;
        if md.is_dir() {
            std::fs::remove_dir(&f.path).map_err(refuse)?;
        } else {
            std::fs::remove_file(&f.path).map_err(refuse)?;
        }
        Ok(Vec::new())
    }

    // ------------------------------------------------------------- misc

    fn statfs(&mut self, r: &mut Reader<'_>) -> Reply {
        let fid = r.u32().ok_or(Refused(EINVAL))?;
        self.fid(fid)?;
        // Nothing here needs real numbers: `df` on a share is a curiosity,
        // and a build tool that checks free space wants "plenty".
        let mut w = Writer::new();
        w.u32(0x0102_1997); // V9FS_MAGIC
        w.u32(4096); // bsize
        w.u64(1 << 30); // blocks
        w.u64(1 << 29); // bfree
        w.u64(1 << 29); // bavail
        w.u64(1 << 20); // files
        w.u64(1 << 19); // ffree
        w.u64(0); // fsid
        w.u32(255); // namelen
        Ok(w.done())
    }

    fn getlock(&mut self, r: &mut Reader<'_>) -> Reply {
        let (Some(fid), Some(_kind), Some(start), Some(length), Some(proc_id), Some(client)) =
            (r.u32(), r.u8(), r.u64(), r.u64(), r.u32(), r.string())
        else {
            return Err(Refused(EINVAL));
        };
        self.fid(fid)?;
        let mut w = Writer::new();
        w.u8(2); // F_UNLCK: the range is free
        w.u64(start);
        w.u64(length);
        w.u32(proc_id);
        w.string(&client);
        Ok(w.done())
    }

    // ---------------------------------------------------------- helpers

    fn fid(&self, fid: u32) -> Result<&Fid, Refused> {
        self.fids.get(&fid).ok_or(Refused(EBADF))
    }

    fn fid_mut(&mut self, fid: u32) -> Result<&mut Fid, Refused> {
        self.fids.get_mut(&fid).ok_or(Refused(EBADF))
    }

    /// `dir/name` for a name the guest wants to create or remove: one
    /// component, never `.`/`..`, never a path.
    fn child(&self, dir: &Path, name: &str) -> Result<PathBuf, Refused> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(Refused(EINVAL));
        }
        if name.contains('\0') {
            return Err(Refused(EINVAL));
        }
        if name.len() > 255 {
            return Err(Refused(ENAMETOOLONG));
        }
        Ok(dir.join(name))
    }
}

fn lstat(path: &Path) -> Result<Metadata, Refused> {
    std::fs::symlink_metadata(path).map_err(refuse)
}

/// Linux open flags → `OpenOptions`. Creation is `Tlcreate`'s business, so
/// `O_CREAT`/`O_EXCL` are not honoured here.
fn open_flags(flags: u32) -> OpenOptions {
    let mut o = OpenOptions::new();
    let acc = flags & O_ACCMODE;
    o.read(acc != O_WRONLY);
    o.write(acc == O_WRONLY || acc == O_RDWR);
    if flags & O_APPEND != 0 {
        o.append(true);
    }
    if flags & O_TRUNC != 0 && acc != 0 {
        o.truncate(true);
    }
    o
}

/// The Linux errno for a host error. By kind, because the host's own numbers
/// are not the guest's: macOS says 66 for a non-empty directory and Linux
/// says 39, and a raw number crossing unchanged would be a lie the guest
/// then prints.
fn errno_for(e: &io::Error) -> u32 {
    use io::ErrorKind as K;
    // No stable kind for a symlink loop yet; the host's number is checked
    // against the host's own constant, and the guest's number goes out.
    if e.raw_os_error() == Some(nix::libc::ELOOP) {
        return ELOOP;
    }
    match e.kind() {
        K::NotFound => ENOENT,
        K::PermissionDenied => EACCES,
        K::AlreadyExists => EEXIST,
        K::NotADirectory => ENOTDIR,
        K::IsADirectory => EISDIR,
        K::DirectoryNotEmpty => ENOTEMPTY,
        K::InvalidInput | K::InvalidData => EINVAL,
        K::StorageFull => ENOSPC,
        K::ReadOnlyFilesystem => EROFS,
        K::InvalidFilename => ENAMETOOLONG,
        K::Unsupported => EOPNOTSUPP,
        _ => EIO,
    }
}

fn refuse(e: io::Error) -> Refused {
    Refused(errno_for(&e))
}

// ------------------------------------------------------------ wire codec

/// A complete message: `size[4] type[1] tag[2]` then the body.
fn message(kind: u8, tag: u16, body: &[u8]) -> Vec<u8> {
    let size = HEADER as usize + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn u32_bytes(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn qid_bytes(qid: Qid) -> Vec<u8> {
    let mut w = Writer::new();
    w.qid(qid);
    w.done()
}

struct Writer(Vec<u8>);

impl Writer {
    fn new() -> Writer {
        Writer(Vec::new())
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn string(&mut self, s: &str) {
        let b = s.as_bytes();
        let n = b.len().min(u16::MAX as usize);
        self.u16(n as u16);
        self.0.extend_from_slice(&b[..n]);
    }
    fn qid(&mut self, q: Qid) {
        self.u8(q.kind);
        self.u32(q.version);
        self.u64(q.path);
    }
    fn done(self) -> Vec<u8> {
        self.0
    }
}

/// A bounds-checked reader: every accessor is `None` past the end, so a
/// truncated request cannot index out of range.
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, at: 0 }
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.buf.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.bytes(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.bytes(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.bytes(8).map(|b| {
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            u64::from_le_bytes(a)
        })
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        let b = self.bytes(n)?;
        Some(String::from_utf8_lossy(b).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client the way the guest kernel is one: it builds requests with
    /// the same codec and reads the replies back.
    struct Client {
        server: Server,
        tag: u16,
    }

    impl Client {
        fn new(root: &Path) -> Client {
            let mut c = Client {
                server: Server::new(root, 1000, 1000).unwrap(),
                tag: 0,
            };
            let reply = c.call(TVERSION, |w| {
                w.u32(8192);
                w.string(VERSION);
            });
            let mut r = Reader::new(&reply.body);
            assert_eq!(r.u32(), Some(8192));
            assert_eq!(r.string().as_deref(), Some(VERSION));
            let reply = c.call(TATTACH, |w| {
                w.u32(0);
                w.u32(u32::MAX);
                w.string("user");
                w.string("");
                w.u32(1000);
            });
            assert_eq!(reply.kind, TATTACH + 1);
            c
        }

        fn call(&mut self, kind: u8, body: impl FnOnce(&mut Writer)) -> ReplyMsg {
            self.tag = self.tag.wrapping_add(1);
            let mut w = Writer::new();
            body(&mut w);
            let request = message(kind, self.tag, &w.done());
            let reply = self.server.handle(&request);
            let mut r = Reader::new(&reply);
            let size = r.u32().unwrap() as usize;
            assert_eq!(size, reply.len(), "the size field is the message length");
            let kind = r.u8().unwrap();
            let tag = r.u16().unwrap();
            assert_eq!(tag, self.tag, "a reply carries its request's tag");
            ReplyMsg {
                kind,
                body: reply[HEADER as usize..].to_vec(),
            }
        }

        /// Walk `path` from the root into `newfid`; the qids of each step.
        fn walk(&mut self, newfid: u32, names: &[&str]) -> ReplyMsg {
            self.call(TWALK, |w| {
                w.u32(0);
                w.u32(newfid);
                w.u16(names.len() as u16);
                for n in names {
                    w.string(n);
                }
            })
        }

        fn errno(reply: &ReplyMsg) -> Option<u32> {
            (reply.kind == RLERROR).then(|| Reader::new(&reply.body).u32().unwrap())
        }
    }

    struct ReplyMsg {
        kind: u8,
        body: Vec<u8>,
    }

    fn scratch() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hello from the host\n").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.js"), b"console.log(1)\n").unwrap();
        dir
    }

    #[test]
    fn a_walk_finds_a_file_and_reads_it_back() {
        let dir = scratch();
        let mut c = Client::new(dir.path());
        let reply = c.walk(1, &["src", "main.js"]);
        assert_eq!(reply.kind, TWALK + 1);
        let mut r = Reader::new(&reply.body);
        assert_eq!(r.u16(), Some(2), "both steps answered with a qid");
        let reply = c.call(TLOPEN, |w| {
            w.u32(1);
            w.u32(0); // O_RDONLY
        });
        assert_eq!(reply.kind, TLOPEN + 1);
        let reply = c.call(TREAD, |w| {
            w.u32(1);
            w.u64(0);
            w.u32(4096);
        });
        assert_eq!(reply.kind, TREAD + 1);
        let mut r = Reader::new(&reply.body);
        let n = r.u32().unwrap() as usize;
        assert_eq!(r.bytes(n).unwrap(), b"console.log(1)\n");
    }

    #[test]
    fn an_edit_on_the_host_is_what_the_next_read_sees() {
        // The whole reason the share exists: no cache anywhere.
        let dir = scratch();
        let mut c = Client::new(dir.path());
        c.walk(1, &["hello.txt"]);
        c.call(TLOPEN, |w| {
            w.u32(1);
            w.u32(0);
        });
        std::fs::write(dir.path().join("hello.txt"), b"edited\n").unwrap();
        let reply = c.call(TREAD, |w| {
            w.u32(1);
            w.u64(0);
            w.u32(4096);
        });
        let mut r = Reader::new(&reply.body);
        let n = r.u32().unwrap() as usize;
        assert_eq!(r.bytes(n).unwrap(), b"edited\n");
    }

    #[test]
    fn a_missing_first_step_is_enoent_and_a_missing_later_step_is_partial() {
        let dir = scratch();
        let mut c = Client::new(dir.path());
        let reply = c.walk(1, &["nope"]);
        assert_eq!(Client::errno(&reply), Some(ENOENT));
        let reply = c.walk(2, &["src", "nope"]);
        assert_eq!(reply.kind, TWALK + 1);
        assert_eq!(Reader::new(&reply.body).u16(), Some(1), "one step answered");
        // …and fid 2 was never bound.
        let reply = c.call(TCLUNK, |w| w.u32(2));
        assert_eq!(
            Client::errno(&reply),
            None,
            "clunk of an unbound fid is harmless"
        );
    }

    #[test]
    fn dot_dot_cannot_leave_the_root() {
        let dir = scratch();
        let root_ino = std::fs::metadata(dir.path()).unwrap().ino();
        let mut c = Client::new(dir.path());
        let reply = c.walk(1, &["..", "..", ".."]);
        assert_eq!(reply.kind, TWALK + 1);
        let mut r = Reader::new(&reply.body);
        assert_eq!(r.u16(), Some(3));
        for _ in 0..3 {
            let _kind = r.u8();
            let _version = r.u32();
            assert_eq!(r.u64(), Some(root_ino), "every step is still the root");
        }
    }

    #[test]
    fn a_directory_lists_its_entries_with_dot_and_dot_dot() {
        let dir = scratch();
        let mut c = Client::new(dir.path());
        c.walk(1, &[]);
        c.call(TLOPEN, |w| {
            w.u32(1);
            w.u32(0);
        });
        let reply = c.call(TREADDIR, |w| {
            w.u32(1);
            w.u64(0);
            w.u32(8192);
        });
        assert_eq!(reply.kind, TREADDIR + 1);
        let mut r = Reader::new(&reply.body);
        let n = r.u32().unwrap() as usize;
        let mut names = Vec::new();
        let mut inner = Reader::new(r.bytes(n).unwrap());
        while let Some(_kind) = inner.u8() {
            let _version = inner.u32().unwrap();
            let _path = inner.u64().unwrap();
            let _offset = inner.u64().unwrap();
            let _dtype = inner.u8().unwrap();
            names.push(inner.string().unwrap());
        }
        names.sort();
        assert_eq!(names, vec![".", "..", "hello.txt", "src"]);
    }

    #[test]
    fn a_short_readdir_resumes_where_it_left_off() {
        let dir = scratch();
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("file-{i:02}")), b"x").unwrap();
        }
        let mut c = Client::new(dir.path());
        c.walk(1, &[]);
        c.call(TLOPEN, |w| {
            w.u32(1);
            w.u32(0);
        });
        let mut all = Vec::new();
        let mut offset = 0u64;
        loop {
            let reply = c.call(TREADDIR, |w| {
                w.u32(1);
                w.u64(offset);
                w.u32(200); // a few entries at a time
            });
            let mut r = Reader::new(&reply.body);
            let n = r.u32().unwrap() as usize;
            if n == 0 {
                break;
            }
            let mut inner = Reader::new(r.bytes(n).unwrap());
            while let Some(_kind) = inner.u8() {
                inner.u32();
                inner.u64();
                offset = inner.u64().unwrap();
                inner.u8();
                all.push(inner.string().unwrap());
            }
        }
        all.sort();
        all.dedup();
        assert_eq!(
            all.len(),
            24,
            "20 files, hello.txt, src, ., ..: no repeats, no gaps"
        );
    }

    #[test]
    fn create_write_rename_and_remove_reach_the_host_directory() {
        let dir = scratch();
        let mut c = Client::new(dir.path());
        c.walk(1, &["src"]);
        let reply = c.call(TLCREATE, |w| {
            w.u32(1);
            w.string("new.js");
            w.u32(O_RDWR);
            w.u32(0o644);
            w.u32(1000);
        });
        assert_eq!(reply.kind, TLCREATE + 1, "{:?}", Client::errno(&reply));
        let reply = c.call(TWRITE, |w| {
            w.u32(1);
            w.u64(0);
            w.u32(5);
            w.bytes(b"data\n");
        });
        assert_eq!(reply.kind, TWRITE + 1);
        assert_eq!(
            std::fs::read(dir.path().join("src/new.js")).unwrap(),
            b"data\n"
        );
        // renameat: src/new.js -> renamed.js at the root
        c.walk(2, &["src"]);
        c.walk(3, &[]);
        let reply = c.call(TRENAMEAT, |w| {
            w.u32(2);
            w.string("new.js");
            w.u32(3);
            w.string("renamed.js");
        });
        assert_eq!(reply.kind, TRENAMEAT + 1);
        assert!(dir.path().join("renamed.js").exists());
        let reply = c.call(TUNLINKAT, |w| {
            w.u32(3);
            w.string("renamed.js");
            w.u32(0);
        });
        assert_eq!(reply.kind, TUNLINKAT + 1);
        assert!(!dir.path().join("renamed.js").exists());
    }

    #[test]
    fn a_non_empty_directory_is_refused_with_the_linux_errno() {
        // The host may say 66; the guest must hear 39.
        let dir = scratch();
        let mut c = Client::new(dir.path());
        c.walk(1, &[]);
        let reply = c.call(TUNLINKAT, |w| {
            w.u32(1);
            w.string("src");
            w.u32(AT_REMOVEDIR);
        });
        assert_eq!(Client::errno(&reply), Some(ENOTEMPTY));
    }

    #[test]
    fn every_file_belongs_to_the_apps_user() {
        let dir = scratch();
        let mut c = Client::new(dir.path());
        c.walk(1, &["hello.txt"]);
        let reply = c.call(TGETATTR, |w| {
            w.u32(1);
            w.u64(GETATTR_BASIC);
        });
        assert_eq!(reply.kind, TGETATTR + 1);
        let mut r = Reader::new(&reply.body);
        let _valid = r.u64();
        let _qid = r.bytes(13);
        let mode = r.u32().unwrap();
        assert_eq!(mode & 0o170000, 0o100000, "a regular file");
        assert_eq!(r.u32(), Some(1000), "uid is the app's, not the host's");
        assert_eq!(r.u32(), Some(1000));
        let _nlink = r.u64();
        let _rdev = r.u64();
        assert_eq!(r.u64(), Some(20), "size");
    }

    #[test]
    fn truncated_and_unknown_requests_are_errors_not_panics() {
        let dir = scratch();
        let mut server = Server::new(dir.path(), 0, 0).unwrap();
        // Too short for a header at all.
        let reply = server.handle(&[1, 2, 3]);
        assert_eq!(reply[4], RLERROR);
        // A Tread with no body.
        let reply = server.handle(&message(TREAD, 7, &[]));
        assert_eq!(reply[4], RLERROR);
        assert_eq!(Reader::new(&reply[7..]).u32(), Some(EINVAL));
        // A type nobody defined.
        let reply = server.handle(&message(200, 8, &[]));
        assert_eq!(Reader::new(&reply[7..]).u32(), Some(EOPNOTSUPP));
        // xattrs are refused, not faked.
        let reply = server.handle(&message(TXATTRWALK, 9, &[]));
        assert_eq!(Reader::new(&reply[7..]).u32(), Some(EOPNOTSUPP));
    }

    #[test]
    fn the_message_size_is_negotiated_down_to_the_cap() {
        let dir = scratch();
        let mut server = Server::new(dir.path(), 0, 0).unwrap();
        let mut w = Writer::new();
        w.u32(64 << 20);
        w.string(VERSION);
        let reply = server.handle(&message(TVERSION, 1, &w.done()));
        assert_eq!(Reader::new(&reply[7..]).u32(), Some(MAX_MSIZE));
        assert_eq!(server.msize(), MAX_MSIZE);
        // A version we do not speak is answered `unknown`, never faked.
        let mut w = Writer::new();
        w.u32(8192);
        w.string("9P2000.u");
        let reply = server.handle(&message(TVERSION, 2, &w.done()));
        let mut r = Reader::new(&reply[7..]);
        r.u32();
        assert_eq!(r.string().as_deref(), Some("unknown"));
    }
}
