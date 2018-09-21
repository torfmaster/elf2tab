extern crate chrono;
extern crate elf;
extern crate tar;
#[macro_use]
extern crate structopt;

use std::cmp;
use std::fmt::Write as fmtwrite;
use std::fs;
use std::io;
use std::io::{Seek, Write};
use std::mem;
use std::slice;

#[macro_use]
mod util;
mod cmdline;
mod header;
use structopt::StructOpt;

#[repr(C)]
#[derive(Clone, Debug)]
struct Header {
    got_sym_start: u32,
    got_start: u32,
    got_size: u32,

    data_sym_start: u32,
    data_start: u32,
    data_size: u32,

    bss_start: u32,
    bss_size: u32,
    reldata_start: u32,

    stack_size: u32,
}

fn main() {
    let opt = cmdline::Opt::from_args();

    // Create the metadata.toml file needed for the TAB file.
    let mut metadata_toml = String::new();
    write!(
        &mut metadata_toml,
        "tab-version = 1
name = \"{}\"
only-for-boards = \"\"
build-date = {}",
        opt.package_name.clone().unwrap_or(String::new()),
        chrono::prelude::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ).unwrap();

    // Start creating a tar archive which will be the .tab file.
    let tab_name = fs::File::create(&opt.output).expect("Could not create the output file.");
    let mut tab = tar::Builder::new(tab_name);

    // Add the metadata file without creating a real file on the filesystem.
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata_toml.as_bytes().len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tab.append_data(&mut header, "metadata.toml", metadata_toml.as_bytes())
        .unwrap();

    // Iterate all input elfs. Convert them to Tock friendly binaries and then
    // add them to the TAB file.
    for elf_path in opt.input {
        let tbf_path = elf_path.with_extension("tbf");

        let elffile = elf::File::open_path(&elf_path).expect("Could not open the .elf file.");
        // Get output file as both read/write for creating the binary and
        // adding it to the TAB tar file.
        let mut outfile: fs::File = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(tbf_path.clone())
            .unwrap();

        // Do the conversion to a tock binary.
        elf_to_tbf(
            &elffile,
            &mut outfile,
            opt.package_name.clone(),
            opt.verbose,
            opt.stack_size,
            opt.app_heap_size,
            opt.kernel_heap_size,
        ).unwrap();

        // Add the file to the TAB tar file.
        outfile.seek(io::SeekFrom::Start(0)).unwrap();
        tab.append_file(tbf_path.file_name().unwrap(), &mut outfile)
            .unwrap();
        outfile.seek(io::SeekFrom::Start(0)).unwrap();
        tab.append_file(
            tbf_path.with_extension("bin").file_name().unwrap(),
            &mut outfile,
        ).unwrap();
    }
}

/// Convert an ELF file to a TBF (Tock Binary Format) binary file.
///
/// This will place all writeable and executable sections from the ELF file
/// into a binary and prepend a TBF header to it. For all writeable sections,
/// if there is a .rel.X section it will be included at the end with a 32 bit
/// length parameter first.
///
/// Assumptions:
/// - Sections in a segment that is RW and set to be loaded will be in RAM and
///   should count towards minimum required RAM.
/// - Sections that are writeable flash regions include .wfr in their name.
fn elf_to_tbf(
    input: &elf::File,
    output: &mut Write,
    package_name: Option<String>,
    verbose: bool,
    stack_len: u32,
    app_heap_len: u32,
    kernel_heap_len: u32,
) -> io::Result<()> {
    let package_name = package_name.unwrap_or(String::new());

    // Get an array of the sections sorted so we place them in the proper order
    // in the binary.
    let mut sections_sort: Vec<(usize, usize)> = Vec::new();
    for (i, section) in input.sections.iter().enumerate() {
        sections_sort.push((i, section.shdr.offset as usize));
    }
    sections_sort.sort_by_key(|s| s.1);

    // Keep track of how much RAM this app will need.
    let mut minimum_ram_size: u32 = 0;

    // Find the ELF segment for the RAM segment. That will tell us how much
    // RAM we need to reserve for when those are copied into memory.
    for segment in input.phdrs.iter() {
        if segment.progtype == elf::types::PT_LOAD
            && segment.flags.0 == elf::types::PF_W.0 + elf::types::PF_R.0
        {
            minimum_ram_size = segment.memsz as u32;
            break;
        }
    }
    if verbose {
        println!(
            "Min RAM size from sections in ELF: {} bytes",
            minimum_ram_size
        );
    }

    // Add in room the app is asking us to reserve for the stack and heaps to
    // the minimum required RAM size.
    minimum_ram_size += align8!(stack_len) + align4!(app_heap_len) + align4!(kernel_heap_len);

    // Need an array of sections to look for relocation data to include.
    let mut rel_sections: Vec<String> = Vec::new();

    // Iterate the sections in the ELF file to find properties of the app that
    // are required to go in the TBF header.
    let mut writeable_flash_regions_count = 0;

    for s in sections_sort.iter() {
        let section = &input.sections[s.0];

        // Count write only sections as writeable flash regions.
        if section.shdr.name.contains(".wfr") && section.shdr.size > 0 {
            writeable_flash_regions_count += 1;
        }

        // Check write+alloc sections for possible .rel.X sections.
        if section.shdr.flags.0 == elf::types::SHF_WRITE.0 + elf::types::SHF_ALLOC.0 {
            // This section is also one we might need to include relocation
            // data for.
            rel_sections.push(section.shdr.name.clone());
        }
    }
    if verbose {
        println!(
            "Number of writeable flash regions: {}",
            writeable_flash_regions_count
        );
    }

    // Keep track of an index of where we are in creating the app binary.
    let mut binary_index = 0;

    // Now we can create the first pass TBF header. This is mostly to get the
    // size of the header since we have to fill in some of the offsets later.
    let mut tbfheader = header::TbfHeader::new();
    let header_length = tbfheader.create(
        minimum_ram_size,
        writeable_flash_regions_count,
        package_name,
    );
    let protected_region_size = header_length;
    binary_index += protected_region_size;

    // The init function is where the app will start executing, defined as an
    // offset from the end of protected region at the beginning of the app in
    // flash. Typically the protected region only includes the TBF header. To
    // calculate the offset we need to find which section includes the entry
    // function and then determine its offset relative to the end of the
    // protected region.
    let mut init_fn_offset: u32 = 0;

    // Need a place to put the app sections before we know the true TBF header.
    let mut binary: Vec<u8> = Vec::new();

    let mut vtable_offset = 0;
    let mut vtable_size = 0;
    let mut first = true;

    for section in &input.sections {
        if section.shdr.name.contains("symtab") {
            for symbol in input.get_symbols(section) {
                for sym in symbol {
                    if sym.name.contains("unnamed") {
                        vtable_size += sym.size;
                        if first {
                            vtable_offset = sym.value;
                            first = false;
                        }
                    }
                }
            }
        }
    }
    println!("Size {}", vtable_size);
    println!("Offset {}", vtable_offset);

    // Iterate the sections in the ELF file and add them to the binary as needed
    for s in sections_sort.iter() {
        let section = &input.sections[s.0];

        // Determine if this is the section where the entry point is in. If it
        // is, then we need to calculate the correct init_fn_offset.
        if input.ehdr.entry >= section.shdr.addr
            && input.ehdr.entry < (section.shdr.addr + section.shdr.size)
        {
            if verbose {
                println!("Entry point is in {} section", section.shdr.name);
            }
            init_fn_offset = (input.ehdr.entry - section.shdr.addr) as u32
                + (binary_index - protected_region_size) as u32
        }

        // If this is writeable, executable, or allocated, is nonzero length,
        // and is type `PROGBITS` we want to add it to the binary.
        if (section.shdr.flags.0
            & (elf::types::SHF_WRITE.0 + elf::types::SHF_EXECINSTR.0 + elf::types::SHF_ALLOC.0)
            != 0) && section.shdr.shtype == elf::types::SHT_PROGBITS
            && section.shdr.size > 0
        {
            if verbose {
                println!(
                    "Including the {} section at offset {} (0x{:x})",
                    section.shdr.name, binary_index, binary_index
                );
            }
            if align4needed!(binary_index) != 0 {
                println!(
                    "Warning! Placing section {} at 0x{:x}, which is not 4-byte aligned.",
                    section.shdr.name, binary_index
                );
            }

            if section.shdr.name.contains("crt0") {
                let len = section.data.len();
                println!("crt0 len: {}", len);
                let mut header = unsafe {
                    let x = &mut *(section.data.as_slice().as_ptr() as *mut Header);
                    x.clone()
                };

                // header.got_sym_start = 3; // crashes
                //header.got_start = 3; // writes to got_size
                //header.got_size = 3; // writes to  data_start
                //header.data_sym_start = 3; // writes to bss_start
                // header.data_start = 3; // crashes
                //header.data_size = 3; // nirvana
                //header.bss_start = 3;
                println!("vtable_offset {}", vtable_offset);
                println!("data_start {}", header.data_start);
                header.bss_start = (vtable_offset - header.data_start as u64) as u32;
                header.bss_size = vtable_size as u32;
                //header.reldata_start = 3;
                //header.stack_size = 3;

                let header_slice =
                    unsafe { slice::from_raw_parts((&header as *const _) as *const u8, len) };
                println!("Written bytes: {}", header_slice.len());
                binary.extend(header_slice);
            } else {
                binary.extend(&section.data);
            }
            // Check if this is a writeable flash region. If so, we need to
            // set the offset and size in the header.
            if section.shdr.name.contains(".wfr") && section.shdr.size > 0 {
                tbfheader.set_writeable_flash_region_values(
                    binary_index as u32,
                    section.shdr.size as u32,
                );
            }

            // Now increment where we are in the binary.
            binary_index += section.shdr.size as usize;
        }
    }

    // Now that we have checked all of the sections, we can set the
    // init_fn_offset.
    tbfheader.set_init_fn_offset(init_fn_offset);

    // Next we have to add in any relocation data.
    let mut relocation_binary: Vec<u8> = Vec::new();

    // For each section that might have relocation data, check if a .rel.X
    // section exists and if so include it.
    if verbose {
        println!("Searching for .rel.X sections to add.");
    }
    for relocation_section_name in rel_sections.iter() {
        let mut name: String = ".rel".to_owned();
        name.push_str(relocation_section_name);

        let rel_data = input
            .sections
            .iter()
            .find(|section| section.shdr.name == name)
            .map(|section| section.data.as_ref())
            .unwrap_or(&[] as &[u8]);

        relocation_binary.extend(rel_data);

        if verbose && rel_data.len() > 0 {
            println!(
                "  Adding {} section. Length: {} bytes at {} (0x{:x})",
                name,
                rel_data.len(),
                binary_index + mem::size_of::<u32>() + rel_data.len(),
                binary_index + mem::size_of::<u32>() + rel_data.len()
            );
        }
        if rel_data.len() > 0 && align4needed!(binary_index) != 0 {
            println!(
                "Warning! Placing section {} at 0x{:x}, which is not 4-byte aligned.",
                name, binary_index
            );
        }
    }

    // Add the relocation data to our total length. Also include the 4 bytes for
    // the relocation data length.
    binary_index += relocation_binary.len() + mem::size_of::<u32>();

    // That is everything that we are going to include in our app binary. Now
    // we need to pad the binary to a power of 2 in size, and make sure it is
    // at least 512 bytes in size.
    let post_content_pad = if binary_index.count_ones() > 1 {
        let power2len = cmp::max(1 << (32 - (binary_index as u32).leading_zeros()), 512);
        power2len - binary_index
    } else {
        0
    };
    binary_index += post_content_pad;
    let total_size = binary_index;

    // Now set the total size of the app in the header.
    tbfheader.set_total_size(total_size as u32);

    if verbose {
        print!("{}", tbfheader);
    }

    // Write the header and actual app to a binary file.
    output.write_all(tbfheader.generate().unwrap().get_ref())?;
    output.write_all(binary.as_ref())?;

    let rel_data_len: [u8; 4] = [
        (relocation_binary.len() & 0xff) as u8,
        (relocation_binary.len() >> 8 & 0xff) as u8,
        (relocation_binary.len() >> 16 & 0xff) as u8,
        (relocation_binary.len() >> 24 & 0xff) as u8,
    ];
    output.write_all(&rel_data_len)?;
    output.write_all(relocation_binary.as_ref())?;

    // Pad to get a power of 2 sized flash app.
    util::do_pad(output, post_content_pad as usize)?;

    Ok(())
}
