// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The engine checked against the tools Linux has: sgdisk and sfdisk
//! read the tables it writes, and write tables it reads; fsck.fat,
//! fsck.exfat, mtools and blkid read the filesystems it makes, and
//! fsck-fat's findings are checked against fsck.fat's on volumes
//! broken on purpose. When `NTFS_IMAGES` names a directory, as in CI's
//! unit job, disks with its tables and filesystems are kept there for
//! Windows' chkdsk too (`tools/windows/chkdsk.ps1`).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::table::{End, New, Scheme, Table};
use crate::{Device, Guid, Kind, Rng, exfat, fat, fatck, layout, types};

/// A host tool, from /usr/sbin or the path.
fn tool(name: &str) -> PathBuf {
    for dir in ["/usr/sbin", "/sbin", "/usr/bin", "/bin"] {
        let path = Path::new(dir).join(name);
        if path.exists() {
            return path;
        }
    }
    panic!(
        "{name} is not installed: the disk tests need gdisk, fdisk (util-linux), dosfstools, \
         exfatprogs and mtools (`./ob doctor` says how to install them)"
    );
}

/// Runs `tool` with `args`; its output, and whether it succeeded.
fn run(name: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(tool(name)).args(args).env("LC_ALL", "C").output().unwrap();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// A scratch image of `bytes`, gone when dropped.
struct Image(PathBuf);

impl Image {
    fn new(bytes: u64) -> Image {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dukovo-disk-{}-{n}.img", std::process::id()));
        File::create(&path).unwrap().set_len(bytes).unwrap();
        Image(path)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }

    fn device(&self, sector: u32) -> Device {
        let file = OpenOptions::new().read(true).write(true).open(&self.0).unwrap();
        let len = file.metadata().unwrap().len();
        Device::whole(file, len, sector)
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write(disk: &Device, table: &Table) {
    let writes = crate::plan(disk, table).unwrap();
    crate::apply(disk, &writes).unwrap();
}

/// A partition as sfdisk reads it: (start, size, type, uuid, name).
type Read = (u64, u64, String, String, String);

/// The label sfdisk reads ("gpt" or "dos") and the partitions.
fn sfdisk(image: &Image) -> (String, Vec<Read>) {
    let (ok, text) = run("sfdisk", &["--json", image.path()]);
    assert!(ok, "{text}");
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let table = &json["partitiontable"];
    let field = |p: &serde_json::Value, key: &str| p[key].as_str().unwrap_or_default().to_lowercase();
    let parts = table["partitions"].as_array().map_or_else(Vec::new, |parts| {
        parts
            .iter()
            .map(|p| {
                (
                    p["start"].as_u64().unwrap(),
                    p["size"].as_u64().unwrap(),
                    field(p, "type"),
                    field(p, "uuid"),
                    p["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    });
    (table["label"].as_str().unwrap().to_string(), parts)
}

fn kind(alias: &str, scheme: Scheme) -> Kind {
    types::parse(alias, scheme).unwrap()
}

#[test]
fn sgdisk_and_sfdisk_read_the_gpts_it_writes() {
    for sector in [512u32, 4096] {
        let image = Image::new(1 << 30);
        let disk = image.device(sector);
        let mut rng = Rng::seeded(11);
        let text = r#"{"table":"gpt","partitions":[
            {"type":"bios-boot","size":"1MiB","name":"BIOS boot"},
            {"type":"efi","size":"512MiB","name":"EFI system"},
            {"type":"swap","size":"64MiB"},
            {"type":"dukovo-root","size":"rest","name":"Dukovo root","attributes":["no-automount"]}]}"#;
        let table = layout::apply(&layout::parse(text).unwrap(), sector, disk.sectors(), None, &mut rng).unwrap();
        write(&disk, &table);
        if sector == 512 {
            let (ok, text) = run("sgdisk", &["-v", image.path()]);
            assert!(ok && text.contains("No problems found"), "{text}");
            let (label, parts) = sfdisk(&image);
            assert_eq!(label, "gpt");
            let ours: Vec<(u64, u64, String, String, String)> = table
                .partitions
                .iter()
                .map(|p| (p.first, p.sectors(), p.kind.to_string(), p.guid.lower(), p.name.clone()))
                .collect();
            assert_eq!(parts, ours);
            let (ok, text) = run("sgdisk", &["-i", "4", image.path()]);
            assert!(ok && text.contains("Attribute flags: 8000000000000000"), "{text}");
        }
        // And read back as written.
        let found = crate::read(&disk).unwrap();
        assert!(found.notes.is_empty(), "{:?}", found.notes);
        assert_eq!(found.table.as_ref(), Some(&table));
    }
}

#[test]
fn it_reads_the_gpts_sgdisk_writes() {
    let image = Image::new(256 << 20);
    let (ok, text) = run(
        "sgdisk",
        &[
            "-U", "11111111-2222-4333-8444-555555555555",
            "-n", "1:2048:+100M", "-t", "1:ef00", "-c", "1:EFI",
            "-n", "3:0:+20M", "-t", "3:8200", "-u", "3:01234567-89AB-4CDE-8F01-23456789ABCD",
            "-n", "2:0:0", "-t", "2:0700", "-c", "2:Données",
            image.path(),
        ],
    );
    assert!(ok, "{text}");
    let found = crate::read(&image.device(512)).unwrap();
    let table = found.table.unwrap();
    assert_eq!(table.disk_guid, Guid::known("11111111-2222-4333-8444-555555555555"));
    let parts: Vec<(u32, u64, Option<&str>, String)> =
        table.partitions.iter().map(|p| (p.number, p.sectors(), p.kind.alias(), p.name.clone())).collect();
    assert_eq!(parts[0], (1, 204_800, Some("efi"), "EFI".to_string()));
    assert_eq!(parts[1].2, Some("windows-data"));
    assert_eq!(parts[1].3, "Données");
    assert_eq!(parts[2], (3, 40_960, Some("swap"), String::new()));
    assert_eq!(table.get(3).unwrap().guid, Guid::known("01234567-89AB-4CDE-8F01-23456789ABCD"));
    assert!(table.verify().is_empty(), "{:?}", table.verify());
}

#[test]
fn a_damaged_copy_is_read_from_the_other_and_mended() {
    let image = Image::new(64 << 20);
    let disk = image.device(512);
    let mut rng = Rng::seeded(12);
    let mut table = Table::new(Scheme::Gpt, 512, disk.sectors(), &mut rng).unwrap();
    table.add(New::of(kind("linux", Scheme::Gpt), End::Rest), &mut rng).unwrap();
    write(&disk, &table);
    // The primary header and then the backup's entries are spoilt.
    disk.write_at(512 + 40, &[0xff; 4]).unwrap();
    let found = crate::read(&disk).unwrap();
    assert!(found.notes[0].contains("primary copy is damaged"), "{:?}", found.notes);
    assert_eq!(found.table.as_ref(), Some(&table));
    write(&disk, &found.table.unwrap());
    let (ok, text) = run("sgdisk", &["-v", image.path()]);
    assert!(ok && text.contains("No problems found"), "{text}");
    let last = disk.sectors() - 1;
    disk.write_at((last - 32) * 512, &[0xff; 16]).unwrap();
    let found = crate::read(&disk).unwrap();
    assert!(found.notes[0].contains("backup copy is damaged"), "{:?}", found.notes);
    // Both gone: no table can be read.
    disk.write_at(512 + 40, &[0xff; 4]).unwrap();
    assert!(matches!(crate::read(&disk), Err(crate::ReadError::Damaged(_))));
}

#[test]
fn a_disk_that_grew_gets_its_backup_moved_to_the_end() {
    let image = Image::new(32 << 20);
    let mut rng = Rng::seeded(13);
    {
        let disk = image.device(512);
        let mut table = Table::new(Scheme::Gpt, 512, disk.sectors(), &mut rng).unwrap();
        table.add(New::of(kind("linux", Scheme::Gpt), End::Sectors(2048 * 8)), &mut rng).unwrap();
        write(&disk, &table);
    }
    File::options().write(true).open(&image.0).unwrap().set_len(64 << 20).unwrap();
    let disk = image.device(512);
    let found = crate::read(&disk).unwrap();
    assert!(found.notes.iter().any(|n| n.contains("larger than the table says")), "{:?}", found.notes);
    let mut table = found.table.unwrap();
    assert_eq!(table.last_usable, disk.sectors() - 34);
    table.add(New::of(kind("swap", Scheme::Gpt), End::Rest), &mut rng).unwrap();
    write(&disk, &table);
    let (ok, text) = run("sgdisk", &["-v", image.path()]);
    assert!(ok && text.contains("No problems found"), "{text}");
}

#[test]
fn mbrs_with_logical_partitions_read_and_keep_them() {
    let image = Image::new(128 << 20);
    let script = "label: dos\nlabel-id: 0xdeadbeef\n\
        start=2048, size=20480, type=c, bootable\n\
        start=22528, size=163840, type=5\n\
        start=24576, size=20480, type=83\n\
        start=47104, size=20480, type=82\n";
    let out = Command::new(tool("sfdisk"))
        .arg(image.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(script.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let disk = image.device(512);
    let mut table = crate::read(&disk).unwrap().table.unwrap();
    assert_eq!(table.signature, 0xdead_beef);
    let parts: Vec<(u32, u64, u64, String, bool, bool)> = table
        .partitions
        .iter()
        .map(|p| (p.number, p.first, p.sectors(), p.kind.to_string(), p.bootable, p.logical))
        .collect();
    assert_eq!(
        parts,
        vec![
            (1, 2048, 20480, "0x0c".into(), true, false),
            (2, 22528, 163_840, "0x05".into(), false, false),
            (5, 24576, 20480, "0x83".into(), false, true),
            (6, 47104, 20480, "0x82".into(), false, true),
        ]
    );
    assert!(table.set_kind(5, Kind::Mbr(0x82)).unwrap_err().0.contains("logical"));
    assert!(layout::dump(&table).is_err());
    let mut rng = Rng::seeded(14);
    table.add(New::of(Kind::Mbr(0x07), End::Rest), &mut rng).unwrap();
    write(&disk, &table);
    let (label, parts) = sfdisk(&image);
    assert_eq!(label, "dos");
    let starts: Vec<(u64, u64, String)> = parts.iter().map(|p| (p.0, p.1, p.2.clone())).collect();
    assert_eq!(
        starts,
        vec![
            (2048, 20480, "c".into()),
            (22528, 163_840, "5".into()),
            (186_368, 75_776, "7".into()),
            (24576, 20480, "83".into()),
            (47104, 20480, "82".into()),
        ]
    );
}

#[test]
fn an_mbr_over_a_gpt_wipes_the_gpts_headers() {
    let image = Image::new(64 << 20);
    let disk = image.device(512);
    let mut rng = Rng::seeded(15);
    let mut gpt = Table::new(Scheme::Gpt, 512, disk.sectors(), &mut rng).unwrap();
    gpt.add(New::of(kind("linux", Scheme::Gpt), End::Rest), &mut rng).unwrap();
    write(&disk, &gpt);
    let mut mbr = Table::new(Scheme::Mbr, 512, disk.sectors(), &mut rng).unwrap();
    mbr.add(New::of(Kind::Mbr(0x83), End::Sectors(2048)), &mut rng).unwrap();
    let writes = crate::plan(&disk, &mbr).unwrap();
    let what: Vec<&str> = writes.iter().map(|w| w.what.as_str()).collect();
    assert_eq!(
        what,
        [
            "zeros over the old GPT's header at sector 1",
            &format!("zeros over the old GPT's header at sector {}", disk.sectors() - 1),
            "the MBR, at sector 0"
        ]
    );
    crate::apply(&disk, &writes).unwrap();
    let (ok, text) = run("sgdisk", &["-v", image.path()]);
    assert!(!text.contains("EFI PART") || !ok || text.contains("MBR"), "{text}");
    let (label, parts) = sfdisk(&image);
    assert_eq!((label.as_str(), parts.len()), ("dos", 1));
}

#[test]
fn a_table_over_a_whole_disk_filesystem_wipes_its_marks() {
    let image = Image::new(64 << 20);
    let disk = image.device(512);
    // Btrfs's and swap's marks, as a filesystem made on the whole disk
    // leaves them.
    disk.write_at((64 << 10) + 64, b"_BHRfS_M").unwrap();
    disk.write_at(4086, b"SWAPSPACE2").unwrap();
    let found = crate::read(&disk).unwrap();
    let names: Vec<&str> = found.signatures.iter().map(|s| s.name).collect();
    assert_eq!(names, ["btrfs", "swap"]);
    let mut rng = Rng::seeded(16);
    let mut table = Table::new(Scheme::Gpt, 512, disk.sectors(), &mut rng).unwrap();
    table.add(New::of(kind("linux", Scheme::Gpt), End::Rest), &mut rng).unwrap();
    write(&disk, &table);
    assert!(crate::read(&disk).unwrap().signatures.is_empty());
}

#[test]
fn fat_volumes_check_and_read_with_dosfstools_and_mtools() {
    // (size, sector, options, bits)
    let cases: [(u64, u32, fat::Options, u8); 5] = [
        (1_474_560, 512, fat::Options { label: Some("floppy".into()), ..Default::default() }, 12),
        (64 << 20, 512, fat::Options { label: Some("Sixteen".into()), ..Default::default() }, 16),
        (512 << 20, 512, fat::Options { label: Some("EFI".into()), ..Default::default() }, 32),
        (40 << 20, 512, fat::Options { bits: Some(32), ..Default::default() }, 32),
        (300 << 20, 4096, fat::Options { bits: Some(32), ..Default::default() }, 32),
    ];
    for (size, sector, options, bits) in cases {
        let image = Image::new(size);
        let device = image.device(sector);
        let mut rng = Rng::seeded(size);
        let made = fat::format(&device, &options, &mut rng).unwrap();
        assert_eq!(made.bits, bits, "{size}");
        let (ok, text) = run("fsck.fat", &["-n", "-v", image.path()]);
        assert!(ok, "fsck.fat on a FAT{bits} of {size}: {text}");
        let (ok, text) = run("blkid", &["-p", "-o", "export", image.path()]);
        assert!(ok, "{text}");
        assert!(text.contains(&format!("UUID={}", fat::volume_id_text(made.volume_id))), "{text}");
        if let Some(label) = &options.label {
            assert!(text.contains(&format!("LABEL={}", label.to_uppercase())), "{text}");
        }
        if sector == 512 {
            // mtools writes a file in and reads it back.
            let file = Image::new(0);
            std::fs::write(&file.0, b"Dukovo made this volume.\n".repeat(400)).unwrap();
            let (ok, text) = run("mcopy", &["-i", image.path(), file.path(), "::/NOTE.TXT"]);
            assert!(ok, "{text}");
            let (ok, text) = run("mtype", &["-i", image.path(), "::/NOTE.TXT"]);
            assert!(ok && text.starts_with("Dukovo made this volume."), "{text}");
            let (ok, text) = run("fsck.fat", &["-n", image.path()]);
            assert!(ok, "{text}");
            let report = fatck::check(&device, fatck::Options::default()).unwrap();
            assert!(report.clean(), "{:?}", report.problems);
            assert_eq!(report.files, 1);
        }
        let report = fatck::check(&device, fatck::Options::default()).unwrap();
        assert!(report.clean(), "{:?}", report.problems);
        assert_eq!(report.bits, bits);
    }
}

/// A FAT volume made by mkfs.fat, with files and directories from
/// mtools.
fn filled(bits: u8, size: u64) -> Image {
    let image = Image::new(size);
    let (ok, text) = run("mkfs.fat", &["-F", &bits.to_string(), "-n", "FILLED", image.path()]);
    assert!(ok, "{text}");
    // Next to the image, whose name no other test shares.
    let dir = std::path::PathBuf::from(format!("{}.files", image.path()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Some Folder/deeper")).unwrap();
    std::fs::write(dir.join("Some Folder/a long file name.txt"), vec![b'a'; 70_000]).unwrap();
    std::fs::write(dir.join("Some Folder/deeper/b.bin"), vec![b'b'; 5000]).unwrap();
    std::fs::write(dir.join("SMALL.TXT"), b"small\n").unwrap();
    std::fs::write(dir.join("empty"), b"").unwrap();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let (ok, text) = run("mcopy", &["-s", "-i", image.path(), path.to_str().unwrap(), "::/"]);
        assert!(ok, "{text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    image
}

/// The directory entry of `name` in a volume's root, its byte offset.
fn root_entry(device: &Device, name: &[u8; 11]) -> u64 {
    let mut boot = [0u8; 512];
    device.read_at(0, &mut boot).unwrap();
    let bpb = fat::Bpb::parse(&boot).unwrap();
    let (at, len) = if bpb.bits == 32 {
        (bpb.cluster_offset(bpb.root_cluster), bpb.cluster_bytes() as usize)
    } else {
        ((bpb.first_data() - u64::from(bpb.root_sectors())) * 512, bpb.root_entries as usize * 32)
    };
    let mut root = vec![0u8; len];
    device.read_at(at, &mut root).unwrap();
    let i = root.as_chunks::<32>().0.iter().position(|e| &e[..11] == name).unwrap();
    at + 32 * i as u64
}

#[test]
fn fsck_fat_finds_and_mends_what_fsck_fat_finds() {
    for bits in [12u8, 16, 32] {
        let size = match bits {
            12 => 4 << 20,
            16 => 32 << 20,
            _ => 40 << 20,
        };
        let image = filled(bits, size);
        let device = image.device(512);
        let clean = fatck::check(&device, fatck::Options::default()).unwrap();
        assert!(clean.clean(), "FAT{bits}: {:?}", clean.problems);
        assert_eq!((clean.files, clean.directories, clean.label.as_deref()), (4, 3, Some("FILLED")));
        let mut boot = [0u8; 512];
        device.read_at(0, &mut boot).unwrap();
        let bpb = fat::Bpb::parse(&boot).unwrap();
        // SMALL.TXT grows past its one cluster, a lost cluster appears
        // near the end, and the second table differs in it.
        let small = root_entry(&device, b"SMALL   TXT");
        device.write_at(small + 28, &(bpb.cluster_bytes() * 3).to_le_bytes()).unwrap();
        let fat_at = u64::from(bpb.reserved) * 512;
        let lost = bpb.clusters - 10;
        let entry = |n: u32| -> (u64, Vec<u8>) {
            match bits {
                12 => {
                    let at = u64::from(n + n / 2);
                    let mut pair = [0u8; 2];
                    device.read_at(fat_at + at, &mut pair).unwrap();
                    let v = u16::from_le_bytes(pair);
                    let v = if n.is_multiple_of(2) { (v & 0xf000) | 0xfff } else { (v & 0xf) | 0xfff0 };
                    (at, v.to_le_bytes().to_vec())
                }
                16 => (u64::from(2 * n), vec![0xff, 0xff]),
                _ => (u64::from(4 * n), vec![0xff, 0xff, 0xff, 0x0f]),
            }
        };
        let (at, bytes) = entry(lost);
        device.write_at(fat_at + at, &bytes).unwrap();
        let (ok, before) = run("fsck.fat", &["-n", image.path()]);
        assert!(!ok, "fsck.fat found nothing wrong on FAT{bits}: {before}");
        let report = fatck::check(&device, fatck::Options::default()).unwrap();
        let said: Vec<&str> = report.problems.iter().map(|(t, _)| t.as_str()).collect();
        assert!(!report.clean());
        assert!(said.iter().any(|t| t.contains("/SMALL.TXT") && t.contains("fewer than its size")), "{said:?}");
        assert!(said.iter().any(|t| t.contains("1 clusters in 1 chain")), "{said:?}");
        assert!(said.iter().any(|t| t.contains("Allocation table 2 differs")), "{said:?}");
        let mended = fatck::check(&device, fatck::Options { repair: true }).unwrap();
        assert!(mended.clean(), "{:?}", mended.problems);
        let (ok, after) = run("fsck.fat", &["-n", image.path()]);
        assert!(ok, "fsck.fat after fsck-fat mended FAT{bits}: {after}");
        let again = fatck::check(&device, fatck::Options::default()).unwrap();
        assert!(again.problems.is_empty(), "{:?}", again.problems);
        let (ok, text) = run("mtype", &["-i", image.path(), "::/SMALL.TXT"]);
        assert!(ok && text.starts_with("small"), "{text}");
    }
}

#[test]
fn fsck_fat_mends_cross_links_and_dirty_volumes() {
    let image = filled(32, 40 << 20);
    let device = image.device(512);
    let mut boot = [0u8; 512];
    device.read_at(0, &mut boot).unwrap();
    let bpb = fat::Bpb::parse(&boot).unwrap();
    // SMALL.TXT's entry takes the long file's first cluster.
    let small = root_entry(&device, b"SMALL   TXT");
    let mut entry = [0u8; 32];
    device.read_at(small, &mut entry).unwrap();
    let folder = root_entry(&device, b"SOMEFO~1   ");
    let mut dir = [0u8; 32];
    device.read_at(folder, &mut dir).unwrap();
    let folder_first = u32::from(u16::from_le_bytes([dir[26], dir[27]]));
    device.write_at(small + 26, &(folder_first as u16).to_le_bytes()).unwrap();
    // Linux's dirty flag, and the FSInfo count off by 100.
    device.write_at(65, &[1]).unwrap();
    let mut info = [0u8; 4];
    device.read_at(512 + 488, &mut info).unwrap();
    let count = u32::from_le_bytes(info) - 100;
    device.write_at(512 + 488, &count.to_le_bytes()).unwrap();
    let report = fatck::check(&device, fatck::Options::default()).unwrap();
    let said: Vec<&str> = report.problems.iter().map(|(t, _)| t.as_str()).collect();
    assert!(said.iter().any(|t| t.contains("meets another file's chain")), "{said:?}");
    assert!(said.iter().any(|t| t.contains("dirty flag")), "{said:?}");
    assert!(said.iter().any(|t| t.contains("FSInfo sector counts")), "{said:?}");
    let mended = fatck::check(&device, fatck::Options { repair: true }).unwrap();
    assert!(mended.clean());
    let (ok, after) = run("fsck.fat", &["-n", image.path()]);
    assert!(ok, "{after}");
    let _ = bpb;
}

#[test]
fn exfat_volumes_check_with_exfatprogs() {
    for (size, sector, cluster) in [(8u64 << 20, 512u32, None), (300 << 20, 512, None), (128 << 20, 4096, Some(64 << 10))] {
        let image = Image::new(size);
        let device = image.device(sector);
        let mut rng = Rng::seeded(size + 1);
        let options = exfat::Options { label: Some("Stick".into()), cluster_bytes: cluster, ..Default::default() };
        let made = exfat::format(&device, &options, &mut rng).unwrap();
        let (ok, text) = run("fsck.exfat", &["-n", "-v", image.path()]);
        assert!(ok, "fsck.exfat on {size} bytes of {sector}-byte sectors: {text}");
        let (ok, text) = run("blkid", &["-p", "-o", "export", image.path()]);
        assert!(ok && text.contains("TYPE=exfat") && text.contains("LABEL=Stick"), "{text}");
        assert!(text.contains(&format!("UUID={}", fat::volume_id_text(made.volume_id))), "{text}");
    }
}

/// Keeps a disk with one partition for Windows' chkdsk, as a fixed VHD,
/// if `NTFS_IMAGES` asks (ADR 0300): its .txt lists nothing, as the
/// volume is empty.
fn keep_for_windows(name: &str, image: &Image) {
    let Ok(dir) = std::env::var("NTFS_IMAGES") else { return };
    std::fs::create_dir_all(&dir).unwrap();
    let path = Path::new(&dir).join(format!("{name}.vhd"));
    std::fs::copy(&image.0, &path).unwrap();
    let size = std::fs::metadata(&image.0).unwrap().len();
    let mut footer = vec![0u8; 512];
    footer[0..8].copy_from_slice(b"conectix");
    footer[8..12].copy_from_slice(&2u32.to_be_bytes());
    footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    footer[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
    footer[28..32].copy_from_slice(b"dkv ");
    footer[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    footer[36..40].copy_from_slice(b"Wi2k");
    footer[40..48].copy_from_slice(&size.to_be_bytes());
    footer[48..56].copy_from_slice(&size.to_be_bytes());
    let cylinders = (size / 512 / (16 * 63)).min(65535) as u16;
    footer[56..58].copy_from_slice(&cylinders.to_be_bytes());
    footer[58] = 16;
    footer[59] = 63;
    footer[60..64].copy_from_slice(&2u32.to_be_bytes());
    footer[68..76].copy_from_slice(&Rng::seeded(size).next_u64().to_be_bytes());
    let sum = footer.iter().fold(0u32, |s, &b| s.wrapping_add(u32::from(b)));
    footer[64..68].copy_from_slice(&(!sum).to_be_bytes());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, &footer).unwrap();
    std::fs::write(Path::new(&dir).join(format!("{name}.txt")), "").unwrap();
}

#[test]
fn disks_for_windows() {
    let cases: [(&str, Scheme, &str, u64); 4] = [
        ("dukovo-gpt-fat32", Scheme::Gpt, "fat32", 96 << 20),
        ("dukovo-mbr-fat16", Scheme::Mbr, "fat16", 48 << 20),
        ("dukovo-mbr-fat12", Scheme::Mbr, "fat12", 8 << 20),
        ("dukovo-gpt-exfat", Scheme::Gpt, "exfat", 96 << 20),
    ];
    for (name, scheme, fs, size) in cases {
        let image = Image::new(size);
        let disk = image.device(512);
        let mut rng = Rng::seeded(size ^ 0x5a);
        let mut table = Table::new(scheme, 512, disk.sectors(), &mut rng).unwrap();
        let alias = match (scheme, fs) {
            (Scheme::Mbr, "fat16") => "fat16",
            (Scheme::Mbr, "fat12") => "fat12",
            _ => "windows-data",
        };
        let mut part = New::of(kind(alias, scheme), End::Rest);
        if scheme == Scheme::Gpt {
            part.name = "Dukovo test".into();
        }
        table.add(part, &mut rng).unwrap();
        write(&disk, &table);
        let p = table.get(1).unwrap();
        let volume = disk.part(p.first, p.sectors()).unwrap();
        if fs == "exfat" {
            exfat::format(&volume, &exfat::Options { label: Some("DUKOVO".into()), ..Default::default() }, &mut rng)
                .unwrap();
        } else {
            let bits = fs[3..].parse().unwrap();
            let options = fat::Options { bits: Some(bits), label: Some("DUKOVO".into()), ..Default::default() };
            fat::format(&volume, &options, &mut rng).unwrap();
            assert!(fatck::check(&volume, fatck::Options::default()).unwrap().problems.is_empty());
        }
        keep_for_windows(name, &image);
    }
}
