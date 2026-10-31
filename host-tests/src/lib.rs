//! machine's tests on the host: the modules with no hardware in them,
//! included as they are (`#[path]`), with the host's std. The crate itself
//! builds only for UEFI, so these are its tests:
//!
//! ```text
//! cd host-tests && cargo test
//! ```

extern crate alloc;

#[path = "../../src/chacha.rs"]
pub mod chacha;
#[path = "../../src/ramfs.rs"]
pub mod ramfs;

#[cfg(test)]
mod tests {
    use super::chacha::chacha20_block;
    use super::ramfs::RamFs;

    // RFC 8439, 2.3.2: the block function's test vector
    #[test]
    fn chacha20_rfc8439_block() {
        let mut key = [0u8; 32];
        for (i, k) in key.iter_mut().enumerate() {
            *k = i as u8;
        }
        let nonce = [0, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let want: [u8; 64] = [
            0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20, 0x71, 0xc4,
            0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03, 0x04, 0x22, 0xaa, 0x9a, 0xc3, 0xd4, 0x6c, 0x4e,
            0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2, 0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2,
            0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9, 0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
        ];
        assert_eq!(chacha20_block(&key, 1, &nonce), want);
    }

    fn read_all(fs: &mut RamFs, path: &[u8]) -> Vec<u8> {
        let fd = fs.open_read(path.to_vec());
        assert!(fd >= 0, "open {}", String::from_utf8_lossy(path));
        let mut out = Vec::new();
        let mut buf = [0u8; 7];
        loop {
            let n = fs.read(fd, &mut buf);
            assert!(n >= 0);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        fs.close(fd);
        out
    }

    // a seek past the end reads nothing, and a write there fills the gap
    // with zeros (it was a panic, a range out of bounds, till 31 October)
    #[test]
    fn seek_past_the_end() {
        let mut fs = RamFs::new();
        fs.put_file(b"a.txt".to_vec(), b"hello".to_vec(), 0);
        let fd = fs.open_read(b"a.txt".to_vec());
        assert_eq!(fs.seek(fd, 10, 0), 10);
        let mut buf = [0u8; 4];
        assert_eq!(fs.read(fd, &mut buf), 0);
        assert_eq!(fs.seek(fd, 0, 2), 5);
        assert_eq!(fs.read(fd, &mut buf), 0);
        assert_eq!(fs.seek(fd, -1, 0), -1);
        fs.close(fd);
        let fd = fs.open_write(b"a.txt".to_vec(), false, 1);
        assert_eq!(fs.seek(fd, 7, 0), 7);
        assert_eq!(fs.write(fd, b"!", 1), 1);
        fs.close(fd);
        assert_eq!(read_all(&mut fs, b"a.txt"), b"hello\0\0!");
    }

    // a tar saved and loaded back: files, folders, modes and times; a path
    // over 100 bytes split into ustar's prefix and name
    #[test]
    fn tar_round_trip() {
        let mut fs = RamFs::new();
        fs.put_file(b"top.txt".to_vec(), b"top".to_vec(), 1_700_000_000);
        fs.put_file(b"dir/sub/deep.bin".to_vec(), (0..=255u8).collect(), 5);
        assert!(fs.chmod(b"dir/sub/deep.bin", 0o755));
        let long = [&[b'd'; 90][..], b"/", &[b'f'; 60][..]].concat();
        fs.put_file(long.clone(), b"long".to_vec(), 6);
        fs.make_dirs(b"empty", 7);
        let tar = fs.save_tar();
        assert_eq!(tar.len() % 512, 0);
        let mut back = RamFs::new();
        assert_eq!(back.load_tar(&tar), Some(3));
        assert_eq!(read_all(&mut back, b"top.txt"), b"top");
        assert_eq!(read_all(&mut back, b"dir/sub/deep.bin"), (0..=255u8).collect::<Vec<_>>());
        assert_eq!(read_all(&mut back, &long), b"long");
        assert!(back.is_dir(b"empty") && back.is_dir(b"dir/sub"));
        let (size, mtime, mode) = back.stat(b"dir/sub/deep.bin").unwrap();
        assert_eq!((mtime, size, mode & 0o777), (5, 256, 0o755));
        assert_eq!(back.stat(b"top.txt").unwrap().1, 1_700_000_000);
        // the host's own tar reads it, as the comments say
        assert_eq!(&tar[257..262], b"ustar");
    }

    // what is not a ustar archive is said to be none; a short one, whose
    // file runs past the end, too, rather than a panic
    #[test]
    fn tar_not_one() {
        let mut fs = RamFs::new();
        assert_eq!(fs.load_tar(&[1u8; 1024]), None);
        let mut whole = RamFs::new();
        whole.put_file(b"f".to_vec(), vec![9; 2000], 0);
        let tar = whole.save_tar();
        assert_eq!(fs.load_tar(&tar[..1024]), None);
        assert_eq!(fs.load_tar(&[]), Some(0));
    }

    // an overlay: what changed since tracking started, saved, and put back
    // over the base, its removals with it
    #[test]
    fn overlay_round_trip() {
        let mut base = RamFs::new();
        base.put_file(b"keep.txt".to_vec(), b"k".to_vec(), 0);
        base.put_file(b"gone.txt".to_vec(), b"g".to_vec(), 0);
        base.put_file(b"edit.txt".to_vec(), b"old".to_vec(), 0);
        let base_tar = base.save_tar();
        base.start_tracking();
        let g0 = base.generation();
        assert!(base.unlink(b"gone.txt"));
        let fd = base.open_write(b"edit.txt".to_vec(), true, 3);
        assert_eq!(base.write(fd, b"new", 3), 3);
        base.close(fd);
        base.put_file(b"new/made.txt".to_vec(), b"m".to_vec(), 4);
        assert!(base.generation() > g0);
        let overlay = base.save_overlay();
        let mut again = RamFs::new();
        again.load_tar(&base_tar).unwrap();
        assert_eq!(again.load_overlay(&overlay), Some((2, 1)));
        assert_eq!(read_all(&mut again, b"keep.txt"), b"k");
        assert_eq!(read_all(&mut again, b"edit.txt"), b"new");
        assert_eq!(read_all(&mut again, b"new/made.txt"), b"m");
        assert!(again.get(b"gone.txt").is_none());
    }

    // paths: . and .. taken out, never above the top
    #[test]
    fn normalize_paths() {
        use super::ramfs::normalize;
        assert_eq!(normalize(b"a/./b//c"), b"a/b/c");
        assert_eq!(normalize(b"a/b/../c"), b"a/c");
        assert_eq!(normalize(b"../../x"), b"x");
        assert_eq!(normalize(b"/tmp/../y"), b"/y");
    }
}
