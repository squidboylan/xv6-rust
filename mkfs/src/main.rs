#[macro_use]
extern crate lazy_static;

use std::cmp::min;
use std::env::args;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::io::SeekFrom;
use std::mem::size_of;
use std::process::exit;
use std::ptr::copy;
use std::slice::from_raw_parts_mut;
use std::sync::Mutex;

// Stuff from stat.h
const T_DIR: u16 = 1; // Directory
const T_FILE: u16 = 2; // File

// Stuff from param.h
const FSSIZE: u32 = 2000; // size of file system in blocks
const MAXOPBLOCKS: u32 = 10; // max # of blocks any FS op writes
const LOGSIZE: u32 = (MAXOPBLOCKS * 3); // max data blocks in on-disk log

// Stuff from fs.h
const NINODES: u32 = 200;
const ROOTINO: u32 = 1; // root i-number
const BSIZE: u32 = 512; // block size

const NINDIRECT: u32 = BSIZE / 4;
const MAXFILE: u32 = NDIRECT + NINDIRECT;

#[repr(C)]
#[derive(Default, Debug, Clone)]
struct superblock {
    size: u32,       // Size of file system image (blocks)
    nblocks: u32,    // Number of data blocks
    ninodes: u32,    // Number of inodes.
    nlog: u32,       // Number of log blocks
    logstart: u32,   // Block number of first log block
    inodestart: u32, // Block number of first inode block
    bmapstart: u32,  // Block number of first free map block
}

const NDIRECT: u32 = 12;

// On-disk inode structure
#[repr(C)]
#[derive(Default, Debug, Clone)]
struct dinode {
    f_type: u16,                        // File type
    major: u16,                         // Major device number (T_DEV only)
    minor: u16,                         // Minor device number (T_DEV only)
    nlink: u16,                         // Number of links to inode in file system
    size: u32,                          // Size of file (bytes)
    addrs: [u32; NDIRECT as usize + 1], // Data block addresses
}

// Block containing inode i
macro_rules! IBLOCK {
    ($i:ident, $sb:ident) => {
        $i as u32 / *IPB as u32 + $sb.inodestart as u32
    };
}

// Directory is a file containing a sequence of dirent structures.
const DIRSIZ: usize = 14;

#[repr(C)]
#[derive(Default, Debug, Clone)]
struct dirent {
    inum: u16,
    // These are C chars, rust has 4 byte chars (for unicode and such)
    name: [u8; DIRSIZ],
}

const NBITMAP: u32 = FSSIZE / (BSIZE as u32 * 8) + 1;

const NLOG: u32 = LOGSIZE;

lazy_static! {
    // Inodes per block.
    static ref IPB: u32 = BSIZE as u32/(size_of::<dinode>() as u32);
    static ref NINODEBLOCKS: u32 = NINODES / *IPB + 1;
    static ref FREEINODE: Mutex<u32> = Mutex::new(1);
    static ref SB: Mutex<superblock> = Mutex::new(
        superblock {
            size: 0,
            nblocks: 0,
            ninodes: 0,
            nlog: 0,
            logstart: 0,
            inodestart: 0,
            bmapstart: 0,

        }
    );
    static ref FREEBLOCK: Mutex<u32> = Mutex::new(0);
}

// convert to intel byte order
fn xu16(src: u16) -> u16 {
    let mut dest: u16 = 0;
    let ptr: *mut u8 = &mut dest as *mut u16 as *mut u8;
    unsafe {
        ptr.offset(0).write(src as u8);
        ptr.offset(1).write((src >> 8) as u8);
    }
    dest
}

// TODO: FIX THESE TESTS
// These tests are bad and only work on intel architecture (little endian)
#[test]
fn test_xu16() {
    assert_eq!(xu16(10), 10);
    assert_eq!(xu16(500), 500);
}

fn xu32(src: u32) -> u32 {
    let mut dest: u32 = 0;
    let ptr: *mut u8 = &mut dest as *mut u32 as *mut u8;
    unsafe {
        ptr.offset(0).write(src as u8);
        ptr.offset(1).write((src >> 8) as u8);
        ptr.offset(2).write((src >> 16) as u8);
        ptr.offset(3).write((src >> 24) as u8);
    }
    dest
}

// TODO: FIX THESE TESTS
// These tests are bad and only work on intel architecture (little endian)
#[test]
fn test_xu32() {
    assert_eq!(xu32(10), 10);
    assert_eq!(xu32(500), 500);
}

// This takes a &mut T, then gets a mut slice of u8 of length size_of::<T>()
// This is useful for serializing data to put onto disk. I think this looks scarier than it really
// is, we're converting to an array of u8 so we cant have alignment issues and our new reference
// cant outlive the passed in &mut T
fn get_mut_u8_slice<'a, T>(data: &'a mut T) -> &'a mut [u8] {
    unsafe {
        from_raw_parts_mut(data as *mut T as *mut u8, size_of::<T>())
    }
}

fn main() {
    let rootino: u32;
    let mut inum: u32;
    let mut off: u32;
    let mut buf: [u8; BSIZE as usize] = [0; BSIZE as usize];

    let args: Vec<String> = args().collect();

    if args.len() < 2 {
        eprintln!("Usage: mkfs fs.img files...");
        exit(1);
    }

    assert_eq!((BSIZE as usize % size_of::<dinode>()), 0);
    assert_eq!((BSIZE as usize % size_of::<dirent>()), 0);

    //open(argv[1], O_RDWR|O_CREAT|O_TRUNC, 0666);
    let mut fsfd = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&args[1])
        .unwrap();

    println!("{} has been opened", &args[1]);

    // 1 fs block = 1 disk sector
    let nmeta = 2 + NLOG + *NINODEBLOCKS + NBITMAP;
    let nblocks = FSSIZE - nmeta;

    {
        let mut sb = SB.lock().unwrap();
        sb.size = xu32(FSSIZE);
        sb.nblocks = xu32(nblocks);
        sb.ninodes = xu32(NINODES);
        sb.nlog = xu32(NLOG);
        sb.logstart = xu32(2);
        sb.inodestart = xu32(2 + NLOG);
        sb.bmapstart = xu32(2 + NLOG + *NINODEBLOCKS);
    }

    println!("nmeta {} (boot, super, log blocks {} inode blocks {}, bitmap blocks {}) blocks {} total {}",
        nmeta, NLOG, *NINODEBLOCKS, NBITMAP, nblocks, FSSIZE);

    {
        let mut freeblock = FREEBLOCK.lock().unwrap();
        *freeblock = nmeta; // the first free block that we can allocate
    }

    let zeroes: [u8; BSIZE as usize] = [0; BSIZE as usize];
    for i in 0..FSSIZE {
        wsect(&mut fsfd, i, &zeroes);
    }

    println!("zeroed out image");

    unsafe {
        let sb = SB.lock().unwrap();
        println!("locked superblock: {:?}", *sb);
        copy(
            &(*sb) as *const superblock as *const u8,
            &mut buf[0] as *mut u8,
            size_of::<superblock>(),
        );
    }
    println!("writing superblock");
    wsect(&mut fsfd, 1, &buf);

    println!("superblock has been written");

    rootino = ialloc(&mut fsfd, T_DIR);
    assert_eq!(rootino, ROOTINO);

    println!("Allocated /");
    let mut name: [u8; DIRSIZ] = [0; DIRSIZ];
    name[0] = b'.';
    let mut de: dirent = dirent {
        inum: xu16(rootino as u16),
        name,
    };
    println!("prepped .");
    iappend(
        &mut fsfd,
        rootino,
        get_mut_u8_slice(&mut de),
        size_of::<dirent>() as u32,
    );

    let mut name: [u8; DIRSIZ] = [0; DIRSIZ];
    name[0] = b'.';
    name[1] = b'.';
    let mut de: dirent = dirent {
        inum: xu16(rootino as u16),
        name,
    };
    println!("prepped ..");
    iappend(
        &mut fsfd,
        rootino,
        get_mut_u8_slice(&mut de),
        size_of::<dirent>() as u32,
    );

    println!(". and .. have been created");
    for i in 2..args.len() {
        assert!(args[i].find('/') == None);

        let mut fd = File::open(&args[i]).unwrap();

        // Skip leading _ in name when writing to file system.
        // The binaries are named _rm, _cat, etc. to keep the
        // build operating system from trying to execute them
        // in place of system binaries like rm and cat.

        println!("Adding {} to image", args[i]);
        let fname = args[i].as_bytes();
        let start = if fname[0] == b'_' { 1 } else { 0 };

        inum = ialloc(&mut fsfd, T_FILE);

        let mut de: dirent = dirent {
            inum: xu16(inum as u16),
            name: [b'\0'; DIRSIZ],
        };

        // TODO: MAKE THIS CLEANER
        // Hacky strncpy
        for j in start..fname.len() {
            if j == DIRSIZ - 1 {
                break;
            }
            de.name[j - start] = fname[j];
        }

        iappend(
            &mut fsfd,
            rootino,
            get_mut_u8_slice(&mut de),
            size_of::<dirent>() as u32,
        );

        println!("Added dirent");

        let mut cc = fd.read(&mut buf).unwrap() as u32;
        while cc > 0 {
            iappend(&mut fsfd, inum, &mut buf, cc);
            cc = fd.read(&mut buf).unwrap() as u32;
        }
    }

    // fix size of root inode dir
    let mut din = rinode(&mut fsfd, rootino);
    off = xu32(din.size);
    off = ((off / BSIZE) + 1) * BSIZE;
    din.size = xu32(off);
    winode(&mut fsfd, rootino, &din);

    {
        let freeblock = FREEBLOCK.lock().unwrap();
        balloc(&mut fsfd, *freeblock);
    }

    exit(0);
}

fn wsect(f: &mut File, sec: u32, buf: &[u8]) {
    let seek_loc = (sec * BSIZE) as u64;
    if f.seek(SeekFrom::Start(seek_loc)).unwrap() != seek_loc {
        panic!("Failed to seek");
    }

    if f.write(buf).unwrap() != BSIZE as usize {
        panic!("Failed to write buf");
    }
}

fn rsect(f: &mut File, sec: u32, buf: &mut [u8]) {
    let seek_loc = (sec * BSIZE) as u64;
    if f.seek(SeekFrom::Start(seek_loc)).unwrap() != seek_loc {
        panic!("Failed to seek");
    }

    //println!("rsect sec: {}", sec);
    if f.read(buf).unwrap() != BSIZE as usize {
        panic!("Failed to read buf");
    }
}

fn balloc(f: &mut File, used: u32) {
    let sb = SB.lock().unwrap();
    let mut buf: [u8; BSIZE as usize] = [0; BSIZE as usize];
    println!("balloc: first {} blocks have been allocated", used);

    assert!(used < BSIZE * 8);

    for i in 0..used as usize {
        buf[i / 8] |= 0x1 << (i % 8);
    }
    println!("balloc: write bitmap block at sector {}", sb.bmapstart);
    wsect(f, sb.bmapstart, &buf);
}

fn ialloc(f: &mut File, f_type: u16) -> u32 {
    let inum = {
        let mut finode = FREEINODE.lock().unwrap();
        let tmp = *finode;
        *finode += 1;
        tmp
    };

    let f_type = xu16(f_type);
    let nlink = xu16(1);
    let size = xu32(0);

    let mut din = dinode {
        f_type,
        major: 0,
        minor: 0,
        nlink,
        size,
        addrs: [0; NDIRECT as usize + 1],
    };

    winode(f, inum, &mut din);
    inum
}

fn winode(f: &mut File, inum: u32, ip: &dinode) {
    let mut buf: [u8; BSIZE as usize] = [0; BSIZE as usize];
    let bn = {
        let sb = SB.lock().unwrap();
        IBLOCK!(inum, sb)
    };

    rsect(f, bn, &mut buf);

    let dip = &mut buf[0] as &mut u8 as *mut u8 as *mut dinode;
    // This is pretty hacky, OK for C code but would be good to have a better abstraction for rust
    unsafe {
        let dip = dip.add(inum as usize % *IPB as usize);
        (*dip).f_type = ip.f_type;
        (*dip).major = ip.major;
        (*dip).minor = ip.minor;
        (*dip).nlink = ip.nlink;
        (*dip).size = ip.size;
        (*dip).addrs = ip.addrs;
    }
    wsect(f, bn, &buf);
}

fn rinode(f: &mut File, inum: u32) -> dinode {
    let mut buf: [u8; BSIZE as usize] = [0; BSIZE as usize];
    let sb = SB.lock().unwrap();

    let bn = IBLOCK!(inum, sb);
    rsect(f, bn, &mut buf);

    let dip = &mut buf[0] as *mut u8 as *mut dinode;
    // This is pretty hacky, OK for C code but would be good to have a better abstraction for rust
    unsafe {
        let dip = dip.add(inum as usize % *IPB as usize);
        (*dip).clone()
    }
}

fn iappend(f: &mut File, inum: u32, xp: &mut [u8], mut n: u32) {
    let mut p: *mut u8 = &mut xp[0] as *mut u8;
    let mut fbn: u32;
    let mut off: u32;
    let mut n1: u32;
    let mut buf: [u8; BSIZE as usize] = [0; BSIZE as usize];
    let mut indirect: [u32; NINDIRECT as usize] = [0; NINDIRECT as usize];
    let mut x: u32;

    let mut freeblock = FREEBLOCK.lock().unwrap();

    let mut din = rinode(f, inum);
    off = xu32(din.size);
    while n > 0 {
        fbn = off / BSIZE;
        //println!("fbn = {}, MAXFILE = {}", fbn, MAXFILE);
        assert!(fbn < MAXFILE);
        if fbn < NDIRECT {
            if xu32(din.addrs[fbn as usize]) == 0 {
                din.addrs[fbn as usize] = xu32(*freeblock);
                *freeblock += 1;
            }
            x = xu32(din.addrs[fbn as usize]);
        } else {
            if xu32(din.addrs[NDIRECT as usize]) == 0 {
                din.addrs[NDIRECT as usize] = xu32(*freeblock);
                *freeblock += 1;
            }
            rsect(f, xu32(din.addrs[NDIRECT as usize]), get_mut_u8_slice(&mut indirect));
            if indirect[fbn as usize - NDIRECT as usize] == 0 {
                indirect[fbn as usize - NDIRECT as usize] = xu32(*freeblock);
                *freeblock += 1;
                wsect(f, xu32(din.addrs[NDIRECT as usize]), get_mut_u8_slice(&mut indirect));
            }
            x = xu32(indirect[fbn as usize - NDIRECT as usize]);
        }
        n1 = min(n, (fbn + 1) * BSIZE - off);
        rsect(f, x, &mut buf);
        //println!("about to bcopy");
        unsafe {
            bcopy(
                p,
                (&mut buf[0] as *mut u8).add(off as usize - (fbn * BSIZE) as usize),
                n1 as usize,
            );
        }
        //println!("bcopy successful");
        wsect(f, x, &buf);
        n -= n1;
        off += n1;
        unsafe {
            p = p.add(n1 as usize);
        }
    }
    //println!("size = {}", off);
    din.size = xu32(off);
    winode(f, inum, &din);
}

unsafe fn bcopy(source: *mut u8, dest: *mut u8, length: usize) {
    for i in 0..length as isize {
        dest.offset(i).write(*source.offset(i));
    }
}
