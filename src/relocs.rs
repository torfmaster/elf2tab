extern crate byteorder;
extern crate elf;

use byteorder::{ByteOrder, LittleEndian, ReadBytesExt};
use elf::Section;
use std::io::Cursor;
use std::mem;
use std::slice;

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

pub fn replace_crt0(crt0: &Section, rel_start: u32, rel_size: u32) -> Vec<u8> {
    let len = crt0.data.len();
    let mut copied_data = crt0.data.clone();
    println!("crt0 len: {}", len);
    let mut header = unsafe { &mut *(copied_data.as_slice().as_ptr() as *mut Header) };
    header.got_start = rel_start;
    header.got_size = rel_size;
    copied_data
}

pub fn produce_relocs(section: &Section, rel_section: &Section) -> Vec<u8> {
    let size_u32 = mem::size_of::<u32>();
    // check preconditions
    if section.shdr.addralign as usize % size_u32 != 0 {
        panic!("section is not 4-aligned");
    }
    if rel_section.shdr.addralign as usize % size_u32 != 0 {
        panic!("relocation section is not 4-aligned");
    }

    // read relocs
    let mut rdr = Cursor::new(&rel_section.data);
    let mut relocs = vec![0; rel_section.data.len() / size_u32];
    rdr.read_u32_into::<LittleEndian>(&mut relocs).unwrap();

    // read data
    let mut rdr = Cursor::new(&section.data);
    let mut data = vec![0; section.data.len() / size_u32];
    rdr.read_u32_into::<LittleEndian>(&mut data).unwrap();

    let section_load_address = section.shdr.addr as usize;

    let mut symbols_to_relocate = Vec::<u32>::new();
    for i in 0..relocs.len() / 2 {
        let relocated_symbol_address = relocs[i * 2] as usize;
        let relocated_symbol_offset = relocated_symbol_address - section_load_address;
        let symbol_value = data[(relocated_symbol_offset) / 4];

        let relocation_type = get_relocation_type(relocs[i * 2 + 1]);
        if is_text_symbol(symbol_value) && relocation_type == RelocationType::Abs32 {
            symbols_to_relocate.push(relocated_symbol_offset as u32);
        }
    }

    let mut symbols_to_relocate_u8 = vec![0; symbols_to_relocate.len() * mem::size_of::<u32>()];
    LittleEndian::write_u32_into(&symbols_to_relocate, &mut symbols_to_relocate_u8);
    symbols_to_relocate_u8
}

#[derive(Debug, PartialEq)]
enum RelocationType {
    Abs32,
    Other,
}

fn is_text_symbol(address: u32) -> bool {
    (address >> 20) == 0x800;
    address & 0x8_000_0000 != 0
}

fn get_relocation_type(info: u32) -> RelocationType {
    match info & 0xFF {
        2 => RelocationType::Abs32,
        _ => RelocationType::Other,
    }
}
