use std::io::{stdin,BufRead,Write,BufWriter,Seek,SeekFrom};
use std::fs::File;
use pearchive::{pack_files,list_dir,pack_dir,list_dir2,list_dir_nr};
use std::env;
use std::path::Path;
use std::ffi::OsString;

fn read_files_from_stdin() -> Vec<OsString> {
    let mut acc: Vec<_> = stdin().lock().lines().map(|x| x.unwrap().into()).collect();
    acc.sort();
    acc
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("pack-files") => {
            let args = &args[2..];
            let outname = args.get(0).unwrap();
            let files = read_files_from_stdin();
            if !files.iter().all(|f| Path::new(&f).is_file()) {
                println!("not everything is a file");
                std::process::exit(1);
            }
            let mut outfile = File::create(outname).unwrap();
            pack_files(files.as_slice(), &mut outfile).unwrap();
        },
        Some("list-dir") => {
            let args = &args[2..];
            let dirname = args.get(0).unwrap();
            let dir = Path::new(dirname);
            let (dirs, files) = list_dir(dir).unwrap();
            for d in dirs {
                println!("dir {:?}", d);
            }
            for f in files {
                println!("file {:?}", f);
            }
        },
        Some("list-dir2") => {
            let args = &args[2..];
            let dirname = args.get(0).unwrap();
            let dir = Path::new(dirname);
            let (dirs, files) = list_dir2(dir).unwrap();
            for d in dirs {
                println!("dir {:?}", d);
            }
            for f in files {
                println!("file {:?}", f);
            }
        },
        Some("list-dir-nr") => {
            let args = &args[2..];
            let dirname = args.get(0).unwrap();
            let dir = Path::new(dirname);
            let (dirs, files) = list_dir_nr(dir).unwrap();
            for d in dirs {
                println!("dir {:?}", d);
            }
            for f in files {
                println!("file {:?}", f);
            }
        },
        // Some("list-dir-c") => {
        //     let args = &args[2..];
        //     let dirname = args.get(0).unwrap();
        //     let dir = Path::new(dirname);
        //     let (dirs, files) = list_dir_c(dir).unwrap();
        //     for d in dirs {
        //         println!("dir {:?}", d);
        //     }
        //     for f in files {
        //         println!("file {:?}", f);
        //     }
        // }
        Some("pack-dir") => {
            let args = &args[2..];
            let dirname = args.get(0).unwrap();
            let outname = args.get(1).unwrap();
            let mut outfile = File::create(outname).unwrap();
            let outdir = Path::new(dirname);
            pack_dir(outdir, &mut outfile).unwrap();
        }
        // Some("unpack") => { unpack(&args[2..]); },
        _ => {
            println!("pack-files <output-file> < <file-list>");
            println!("pack-dir <input dir> <output file>");
            println!("unpack <input-file> <output-file>");
            println!("list-dir <dir>");
        }
    }
}
