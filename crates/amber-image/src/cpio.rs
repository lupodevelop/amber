//! A tiny newc cpio writer for building initramfs images in memory, gzip-packed.
//! Just enough for amber's bootstrap initramfs: directories, files, symlinks, and
//! character device nodes (so `/dev/console` exists before devtmpfs is mounted).

use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Write;

const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;

pub struct Cpio {
    buf: Vec<u8>,
    ino: u32,
}

impl Default for Cpio {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpio {
    pub fn new() -> Self {
        Cpio {
            buf: Vec::new(),
            ino: 0,
        }
    }

    pub fn dir(&mut self, name: &str, mode: u32) {
        self.record(name, S_IFDIR | mode, &[], (0, 0));
    }

    pub fn file(&mut self, name: &str, data: &[u8], mode: u32) {
        self.record(name, S_IFREG | mode, data, (0, 0));
    }

    pub fn symlink(&mut self, name: &str, target: &str) {
        self.record(name, S_IFLNK | 0o777, target.as_bytes(), (0, 0));
    }

    pub fn char_dev(&mut self, name: &str, mode: u32, major: u32, minor: u32) {
        self.record(name, S_IFCHR | mode, &[], (major, minor));
    }

    /// Append the end-of-archive trailer and return the gzipped bytes.
    pub fn finish_gz(mut self) -> std::io::Result<Vec<u8>> {
        let name = b"TRAILER!!!\0";
        let hdr = header(0, 0, 0, name.len(), (0, 0), 1);
        self.buf.extend_from_slice(hdr.as_bytes());
        self.buf.extend_from_slice(name);
        pad4(&mut self.buf);

        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&self.buf)?;
        gz.finish()
    }

    fn record(&mut self, name: &str, mode: u32, data: &[u8], rdev: (u32, u32)) {
        self.ino += 1;
        let name_z = format!("{name}\0");
        let hdr = header(self.ino, mode, data.len(), name_z.len(), rdev, 1);
        self.buf.extend_from_slice(hdr.as_bytes());
        self.buf.extend_from_slice(name_z.as_bytes());
        pad4(&mut self.buf);
        self.buf.extend_from_slice(data);
        pad4(&mut self.buf);
    }
}

fn header(ino: u32, mode: u32, filesize: usize, namesize: usize, rdev: (u32, u32), nlink: u32) -> String {
    // newc: magic + 13 eight-hex fields.
    let fields = [
        ino,
        mode,
        0, // uid
        0, // gid
        nlink,
        0, // mtime
        filesize as u32,
        0, // devmajor
        0, // devminor
        rdev.0,
        rdev.1,
        namesize as u32,
        0, // check
    ];
    let mut s = String::from("070701");
    for f in fields {
        s.push_str(&format!("{f:08x}"));
    }
    s
}

fn pad4(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}
