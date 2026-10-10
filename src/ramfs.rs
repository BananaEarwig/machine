//! The files, all of them in memory. They are loaded at boot from a tar of
//! the ChrysaLisp folder, and anything written is kept till the machine
//! stops, there is no disk to write it back to yet.
//!
//! A path is a key, as ChrysaLisp gives it with any "./" and trailing "/"
//! taken off, so "apps/tui/tui.lisp". The root is the empty key. A folder
//! is a key of its own, and what is in it is every key that is its key, a
//! "/", and a name with no "/".

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// no file is bigger than this, 1 GB
const MAX_FILE: usize = 1 << 30;

pub const S_IFDIR: u16 = 0o040000;
pub const S_IFREG: u16 = 0o100000;

// what readdir gives as the type, as the Linux host lists them
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

pub enum Node {
    Dir,
    File(Vec<u8>),
}

pub struct Entry {
    pub node: Node,
    /// seconds since 1970
    pub mtime: i64,
    /// who may read, write and run it, 0 for the default
    pub perms: u16,
}

pub struct OpenFile {
    key: Vec<u8>,
    pos: usize,
}

pub struct RamFs {
    entries: BTreeMap<Vec<u8>, Entry>,
    fds: Vec<Option<OpenFile>>,
}

/// the key of a path
pub fn normalize(path: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(path.len());
    for part in path.split(|&c| c == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                // up a folder, if there is one to go up from
                match out.iter().rposition(|&c| c == b'/') {
                    Some(i) => out.truncate(i),
                    None => out.clear(),
                }
            }
            _ => {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(part);
            }
        }
    }
    // "/tmp/x" stays a path from the root of a machine, a different key
    if path.first() == Some(&b'/') {
        out.insert(0, b'/');
    }
    out
}

fn parent(key: &[u8]) -> Option<&[u8]> {
    match key.iter().rposition(|&c| c == b'/') {
        Some(0) => Some(&key[..1]),
        Some(i) => Some(&key[..i]),
        None if key.is_empty() => None,
        None => Some(b""),
    }
}

impl RamFs {
    pub const fn new() -> Self {
        RamFs { entries: BTreeMap::new(), fds: Vec::new() }
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// a folder, and every folder on the way to it
    pub fn make_dirs(&mut self, key: &[u8], mtime: i64) {
        let mut k = key;
        let mut todo: Vec<Vec<u8>> = Vec::new();
        while !k.is_empty() && k != b"/" && !self.entries.contains_key(k) {
            todo.push(k.to_vec());
            k = parent(k).unwrap_or(b"");
        }
        for k in todo {
            self.entries.insert(k, Entry { node: Node::Dir, mtime, perms: 0 });
        }
    }

    /// a file of these bytes, in folders made as they are needed
    pub fn put_file(&mut self, key: Vec<u8>, data: Vec<u8>, mtime: i64) {
        if let Some(p) = parent(&key) {
            self.make_dirs(p, mtime);
        }
        self.entries.insert(key, Entry { node: Node::File(data), mtime, perms: 0 });
    }

    pub fn get(&self, key: &[u8]) -> Option<&Entry> {
        if key.is_empty() || key == b"/" {
            return None;
        }
        self.entries.get(key)
    }

    pub fn is_dir(&self, key: &[u8]) -> bool {
        key.is_empty() || matches!(self.entries.get(key), Some(Entry { node: Node::Dir, .. }))
    }

    pub fn file(&self, key: &[u8]) -> Option<&[u8]> {
        match self.entries.get(key) {
            Some(Entry { node: Node::File(d), .. }) => Some(d),
            _ => None,
        }
    }

    /// size, mtime and mode, as stat gives them
    pub fn stat(&self, key: &[u8]) -> Option<(i64, i64, u16)> {
        if key.is_empty() {
            return Some((0, 0, S_IFDIR | 0o755));
        }
        self.entries.get(key).map(|e| match &e.node {
            Node::Dir => (0, e.mtime, S_IFDIR | if e.perms != 0 { e.perms } else { 0o755 }),
            Node::File(d) => (d.len() as i64, e.mtime, S_IFREG | if e.perms != 0 { e.perms } else { 0o644 }),
        })
    }

    /// the mode of a file, who may read, write and run it
    pub fn chmod(&mut self, key: &[u8], mode: u16) -> bool {
        match self.entries.get_mut(key) {
            Some(e) => {
                e.perms = mode & 0o7777;
                true
            }
            None => false,
        }
    }

    /// the keys of what is in a folder, and if each is a folder
    fn children<'a>(&'a self, key: &'a [u8]) -> impl Iterator<Item = (&'a [u8], bool)> + 'a {
        let mut prefix = key.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let plen = prefix.len();
        self.entries
            .range(prefix.clone()..)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .filter(move |(k, _)| !k[plen..].contains(&b'/'))
            .map(move |(k, e)| (&k[plen..], matches!(e.node, Node::Dir)))
    }

    /// "name,type," for each, as pii_dirlist gives them, with "." and ".."
    pub fn dirlist(&self, key: &[u8], out: &mut Vec<u8>) -> bool {
        if !self.is_dir(key) {
            return false;
        }
        for (name, dir) in [(&b"."[..], true), (&b".."[..], true)].into_iter().chain(self.children(key)) {
            out.extend_from_slice(name);
            out.push(b',');
            out.push(if dir { DT_DIR } else { DT_REG } + b'0');
            out.push(b',');
        }
        true
    }

    /// a file, and all of a folder
    pub fn remove(&mut self, key: &[u8]) -> bool {
        if key.is_empty() || !self.entries.contains_key(key) {
            return false;
        }
        if self.is_dir(key) {
            let mut prefix = key.to_vec();
            prefix.push(b'/');
            let gone: Vec<Vec<u8>> =
                self.entries.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(k, _)| k.clone()).collect();
            for k in gone {
                self.entries.remove(&k);
            }
        }
        self.entries.remove(key);
        true
    }

    pub fn unlink(&mut self, key: &[u8]) -> bool {
        if self.file(key).is_some() {
            self.entries.remove(key);
            true
        } else {
            false
        }
    }

    ///////////////
    // open files, a descriptor is an index, 0 1 and 2 are the console's
    ///////////////

    fn new_fd(&mut self, f: OpenFile) -> i64 {
        if self.fds.len() < 3 {
            self.fds.resize_with(3, || None);
        }
        match self.fds.iter().skip(3).position(|f| f.is_none()) {
            Some(i) => {
                self.fds[i + 3] = Some(f);
                (i + 3) as i64
            }
            None => {
                self.fds.push(Some(f));
                (self.fds.len() - 1) as i64
            }
        }
    }

    fn fd(&mut self, fd: i64) -> Option<&mut OpenFile> {
        if fd < 3 {
            return None;
        }
        self.fds.get_mut(fd as usize)?.as_mut()
    }

    pub fn open_read(&mut self, key: Vec<u8>) -> i64 {
        if self.file(&key).is_none() {
            return -1;
        }
        self.new_fd(OpenFile { key, pos: 0 })
    }

    /// made if it is not there, emptied or not, and where writes go from
    pub fn open_write(&mut self, key: Vec<u8>, truncate: bool, now: i64) -> i64 {
        if key.is_empty() || self.is_dir(&key) {
            return -1;
        }
        let pos = match self.entries.get_mut(&key) {
            Some(Entry { node: Node::File(d), mtime, .. }) => {
                if truncate {
                    d.clear();
                    *mtime = now;
                }
                d.len()
            }
            _ => {
                self.put_file(key.clone(), Vec::new(), now);
                0
            }
        };
        self.new_fd(OpenFile { key, pos })
    }

    pub fn close(&mut self, fd: i64) -> i64 {
        match self.fds.get_mut(fd as usize) {
            Some(f @ Some(_)) if fd >= 3 => {
                *f = None;
                0
            }
            _ => -1,
        }
    }

    pub fn read(&mut self, fd: i64, buf: &mut [u8]) -> i64 {
        let Some(f) = self.fd(fd) else { return -1 };
        let (key, pos) = (f.key.clone(), f.pos);
        let Some(data) = self.file(&key) else { return -1 };
        let n = buf.len().min(data.len().saturating_sub(pos));
        buf[..n].copy_from_slice(&data[pos..pos + n]);
        self.fd(fd).unwrap().pos += n;
        n as i64
    }

    pub fn write(&mut self, fd: i64, buf: &[u8], now: i64) -> i64 {
        let Some(f) = self.fd(fd) else { return -1 };
        let (key, pos) = (f.key.clone(), f.pos);
        let Some(Entry { node: Node::File(data), mtime, .. }) = self.entries.get_mut(&key) else { return -1 };
        // a length that is not one, as a write from a buffer that is not
        // there is on Linux, EFAULT, is an error and not a panic. ChrysaLisp
        // does make such a write, a flush of a stream that has been reset.
        let Some(end) = pos.checked_add(buf.len()).filter(|&e| e <= MAX_FILE) else {
            return -1;
        };
        if data.len() < end {
            data.resize(end, 0);
        }
        data[pos..end].copy_from_slice(buf);
        *mtime = now;
        self.fd(fd).unwrap().pos += buf.len();
        buf.len() as i64
    }

    /// whence 0 from the start, 1 from here, 2 from the end
    pub fn seek(&mut self, fd: i64, offset: i64, whence: u8) -> i64 {
        let Some(f) = self.fd(fd) else { return -1 };
        let (key, pos) = (f.key.clone(), f.pos as i64);
        let len = self.file(&key).map(|d| d.len() as i64).unwrap_or(0);
        let new = match whence {
            0 => offset,
            1 => pos + offset,
            2 => len + offset,
            _ => return -1,
        };
        if new < 0 {
            return -1;
        }
        self.fd(fd).unwrap().pos = new as usize;
        new
    }

    ///////////////
    // a tar, as the folder is packed for the machine
    ///////////////

    /// load a ustar archive, returns how many files, or None if it is not one
    pub fn load_tar(&mut self, tar: &[u8]) -> Option<usize> {
        fn octal(f: &[u8]) -> u64 {
            f.iter()
                .skip_while(|&&c| c == b' ')
                .take_while(|&&c| (b'0'..=b'7').contains(&c))
                .fold(0, |n, &c| n * 8 + (c - b'0') as u64)
        }
        fn cstr(f: &[u8]) -> &[u8] {
            &f[..f.iter().position(|&c| c == 0).unwrap_or(f.len())]
        }
        let mut at = 0usize;
        let mut files = 0;
        while at + 512 <= tar.len() {
            let h = &tar[at..at + 512];
            if h.iter().all(|&c| c == 0) {
                break;
            }
            if &h[257..262] != b"ustar" {
                return None;
            }
            let mut name = cstr(&h[345..500]).to_vec();
            if !name.is_empty() {
                name.push(b'/');
            }
            name.extend_from_slice(cstr(&h[0..100]));
            let size = octal(&h[124..136]) as usize;
            let mtime = octal(&h[136..148]) as i64;
            let perms = (octal(&h[100..108]) & 0o7777) as u16;
            let key = normalize(&name);
            at += 512;
            match h[156] {
                b'0' | 0 => {
                    let data = tar.get(at..at + size)?.to_vec();
                    self.put_file(key.clone(), data, mtime);
                    self.chmod(&key, perms);
                    files += 1;
                }
                b'5' => self.make_dirs(&key, mtime),
                _ => {}
            }
            at += size.div_ceil(512) * 512;
        }
        Some(files)
    }

    /// Everything, as a ustar archive that `load_tar` loads back, and tar
    /// lists: folders and files, with their mtimes and modes. A path too
    /// long for ustar, over 255 bytes or with no '/' to split it at within
    /// 155 and 100, is left out.
    pub fn save_tar(&self) -> Vec<u8> {
        fn octal(f: &mut [u8], v: u64) {
            // digits, then a NUL, as GNU tar writes them
            let n = f.len() - 1;
            let mut v = v;
            for i in (0..n).rev() {
                f[i] = b'0' + (v & 7) as u8;
                v >>= 3;
            }
            f[n] = 0;
        }
        let mut out = Vec::new();
        for (key, e) in &self.entries {
            let mut name = key.clone();
            if matches!(e.node, Node::Dir) {
                name.push(b'/');
            }
            let (prefix, base) = if name.len() <= 100 {
                (&name[..0], &name[..])
            } else {
                // split at a '/' so the prefix is at most 155, the rest 100
                match (0..name.len()).rev().find(|&i| name[i] == b'/' && i <= 155 && name.len() - i - 1 <= 100 && i + 1 < name.len()) {
                    Some(i) => (&name[..i], &name[i + 1..]),
                    None => continue,
                }
            };
            let mut h = [0u8; 512];
            h[..base.len()].copy_from_slice(base);
            h[345..345 + prefix.len()].copy_from_slice(prefix);
            let mode = if e.perms != 0 { e.perms as u64 } else if matches!(e.node, Node::Dir) { 0o755 } else { 0o644 };
            octal(&mut h[100..108], mode);
            octal(&mut h[108..116], 0);
            octal(&mut h[116..124], 0);
            let data: &[u8] = match &e.node {
                Node::File(d) => d,
                Node::Dir => &[],
            };
            octal(&mut h[124..136], data.len() as u64);
            octal(&mut h[136..148], e.mtime.max(0) as u64);
            h[156] = if matches!(e.node, Node::Dir) { b'5' } else { b'0' };
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            // the checksum, of the header with its own field as spaces
            h[148..156].copy_from_slice(b"        ");
            let sum: u64 = h.iter().map(|&b| b as u64).sum();
            octal(&mut h[148..155], sum);
            h[155] = b' ';
            out.extend_from_slice(&h);
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        // the end, two blocks of zeros
        out.resize(out.len() + 1024, 0);
        out
    }
}
