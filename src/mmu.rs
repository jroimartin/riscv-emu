//! Emulated MMU with byte-level memory permissions able to detect unitialized
//! memory accesses.

use std::convert::TryInto;
use std::fmt;
use std::mem;
use std::ops::Deref;

/// Executable memory. Aimed to be used with `Perm`.
pub const PERM_EXEC: u8 = 1;

/// Writable memory. Aimed to be used with `Perm`.
pub const PERM_WRITE: u8 = 1 << 1;

/// Readable memory. Aimed to be used with `Perm`.
pub const PERM_READ: u8 = 1 << 2;

/// Read-after-write memory. Aimed to be used with `Perm`.
///
/// This permission should be set when allocating writable memory. If a memory
/// position has this flag and is written, the READ permission will be
/// automatically assigned afterwards. This allows us to detect accesses to
/// unitialized memory.
pub const PERM_RAW: u8 = 1 << 3;

/// Block size used for resetting and tracking memory which has been modified.
/// Memory is considered dirty after writing to it and after changing its
/// permissions.
pub const DIRTY_BLOCK_SIZE: usize = 1024;

/// Memory error.
#[derive(Debug)]
pub enum Error {
    /// Memory address is out of range.
    InvalidAddress { addr: VirtAddr, size: usize },

    /// Integer overflow when computing address.
    AddressIntegerOverflow { addr: VirtAddr, size: usize },

    /// Read access to unitialized memory.
    UnitializedMemory { addr: VirtAddr },

    /// Permissions do not allow memory access.
    NotAllowed {
        addr: VirtAddr,
        exp_perms: Perm,
        cur_perms: Perm,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::InvalidAddress { addr, size } => {
                write!(f, "invalid address: vaddr={} size={}", addr, size)
            }
            Error::AddressIntegerOverflow { addr, size } => {
                write!(f, "integer overflow: vaddr={} size={}", addr, size)
            }
            Error::UnitializedMemory { addr } => {
                write!(f, "unitialized memory: vaddr={}", addr)
            }
            Error::NotAllowed {
                addr,
                exp_perms,
                cur_perms,
            } => write!(
                f,
                "not allowed: vaddr={} exp={} cur={}",
                addr, exp_perms, cur_perms
            ),
        }
    }
}

/// Memory permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perm(pub u8);

impl fmt::Display for Perm {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut disp = String::new();

        if self.0 & PERM_READ != 0 {
            disp.push('R');
        } else {
            disp.push('-');
        }

        if self.0 & PERM_WRITE != 0 {
            disp.push('W');
        } else {
            disp.push('-');
        }

        if self.0 & PERM_EXEC != 0 {
            disp.push('X');
        } else {
            disp.push('-');
        }

        write!(f, "{}", disp)
    }
}

impl Deref for Perm {
    type Target = u8;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Virtual address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtAddr(pub usize);

impl Deref for VirtAddr {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// Emulated memory management unit.
#[derive(Debug, PartialEq, Eq)]
pub struct Mmu {
    /// Memory size.
    size: usize,

    /// Memory contents.
    memory: Vec<u8>,

    /// Byte-level memory permissions.
    perms: Vec<Perm>,

    /// Block indices in `memory` which are dirty.
    dirty: Vec<usize>,

    /// Tracks which parts of memory have been dirtied.
    dirty_bitmap: Vec<u64>,
}

impl Mmu {
    /// Returns a new Mmu with a given memory `size`.
    ///
    /// # Panics
    ///
    /// This function panics if `size` is lower than `DIRTY_BLOCK_SIZE`.
    pub fn new(size: usize) -> Mmu {
        assert!(size >= DIRTY_BLOCK_SIZE, "invalid size");

        Mmu {
            size,
            memory: vec![0; size],
            perms: vec![Perm(0); size],
            dirty: Vec::with_capacity(size / DIRTY_BLOCK_SIZE + 1),
            dirty_bitmap: vec![0; size / DIRTY_BLOCK_SIZE / 64 + 1],
        }
    }

    /// Returns a copy of the MMU. It marks all memory as clean in the new
    /// copy.
    pub fn fork(&self) -> Mmu {
        Mmu {
            size: self.size,
            memory: self.memory.clone(),
            perms: self.perms.clone(),
            dirty: Vec::with_capacity(self.size / DIRTY_BLOCK_SIZE + 1),
            dirty_bitmap: vec![0; self.size / DIRTY_BLOCK_SIZE / 64 + 1],
        }
    }

    /// Restores memory to the original state `other`.
    pub fn reset(&mut self, other: &Mmu) {
        // Restore memory and set as clean.
        for &block in &self.dirty {
            let start = block * DIRTY_BLOCK_SIZE;
            let end = (block + 1) * DIRTY_BLOCK_SIZE;

            self.dirty_bitmap[block / 64] = 0;
            self.memory[start..end].copy_from_slice(&other.memory[start..end]);
            self.perms[start..end].copy_from_slice(&other.perms[start..end]);
        }
        self.dirty.clear();
    }

    /// Set memory permissions in the given range.
    pub fn set_perms(
        &mut self,
        addr: VirtAddr,
        size: usize,
        perms: Perm,
    ) -> Result<(), Error> {
        let end = addr
            .checked_add(size)
            .ok_or(Error::AddressIntegerOverflow { addr, size })?;

        self.perms
            .get_mut(*addr..end)
            .ok_or(Error::InvalidAddress { addr, size })?
            .iter_mut()
            .for_each(|p| *p = perms);

        self.update_dirty(addr, size);

        Ok(())
    }

    /// Given a memory range and the expected permissions, this function will
    /// return true if every byte in the specified region satisfies those
    /// permissions. Otherwise, the function will return false.
    pub fn check_perms(
        &self,
        addr: VirtAddr,
        size: usize,
        perms: Perm,
    ) -> Result<(), Error> {
        let end = addr
            .checked_add(size)
            .ok_or(Error::AddressIntegerOverflow { addr, size })?;

        let range = self
            .perms
            .get(*addr..end)
            .ok_or(Error::InvalidAddress { addr, size })?;

        for (i, p) in range.iter().enumerate() {
            // If we reach this point, we know that addr + size does not
            // overflow. Given that i < size, we don't need to use
            // checked_add to calculate the following virtual addresses.

            if (*perms & PERM_READ != 0) && (**p & PERM_RAW != 0) {
                return Err(Error::UnitializedMemory {
                    addr: VirtAddr(*addr + i),
                });
            }

            if **p & *perms != *perms {
                return Err(Error::NotAllowed {
                    addr: VirtAddr(*addr + i),
                    exp_perms: perms,
                    cur_perms: *p,
                });
            }
        }

        Ok(())
    }

    /// Copy the bytes in `src` to the given memory address. This function will
    /// fail if the destination memory is not writable.
    pub fn write(&mut self, addr: VirtAddr, src: &[u8]) -> Result<(), Error> {
        let size = src.len();

        // Check if the destination memory range is writable.
        self.check_perms(addr, size, Perm(PERM_WRITE))?;

        let end = addr
            .checked_add(size)
            .ok_or(Error::AddressIntegerOverflow { addr, size })?;

        // Update memory contents
        self.memory
            .get_mut(*addr..end)
            .ok_or(Error::InvalidAddress { addr, size })?
            .copy_from_slice(src);

        // Add PERM_READ and remove PERM_RAW in case of RAW.
        self.perms
            .get_mut(*addr..end)
            .ok_or(Error::InvalidAddress { addr, size })?
            .iter_mut()
            .filter(|p| ***p & PERM_RAW != 0)
            .for_each(|p| *p = Perm((**p | PERM_READ) & !PERM_RAW));

        self.update_dirty(addr, size);

        Ok(())
    }

    /// Copy the data starting in the specified memory address into `dst`.
    /// This function will fail if the source memory is not readable.
    pub fn read(&self, addr: VirtAddr, dst: &mut [u8]) -> Result<(), Error> {
        self.read_with_perms(addr, dst, Perm(PERM_READ))
    }

    /// Copy the data starting in the specified memory address into `dst`.
    /// This function will fail if the source memory does not satisfy the
    /// expected permissions.
    pub fn read_with_perms(
        &self,
        addr: VirtAddr,
        dst: &mut [u8],
        perms: Perm,
    ) -> Result<(), Error> {
        let size = dst.len();

        // Check if the source memory range is readable.
        self.check_perms(addr, size, perms)?;

        let end = addr
            .checked_add(size)
            .ok_or(Error::AddressIntegerOverflow { addr, size })?;

        let src = self
            .memory
            .get(*addr..end)
            .ok_or(Error::InvalidAddress { addr, size })?;

        dst.copy_from_slice(src);

        Ok(())
    }

    /// Copy the bytes in `src` to the given memory address. This function
    /// does not check memory permissions and does not mark memory as dirty.
    pub fn poke(&mut self, addr: VirtAddr, src: &[u8]) -> Result<(), Error> {
        let size = src.len();

        let end = addr
            .checked_add(size)
            .ok_or(Error::AddressIntegerOverflow { addr, size })?;

        self.memory
            .get_mut(*addr..end)
            .ok_or(Error::InvalidAddress { addr, size })?
            .copy_from_slice(src);
        Ok(())
    }

    /// Copy the data starting in the specified memory address into `dst`.
    /// This function does not check memory permissions.
    pub fn peek(&self, addr: VirtAddr, dst: &mut [u8]) -> Result<(), Error> {
        let size = dst.len();

        let end = addr
            .checked_add(size)
            .ok_or(Error::AddressIntegerOverflow { addr, size })?;

        let src = self
            .memory
            .get(*addr..end)
            .ok_or(Error::InvalidAddress { addr, size })?;

        dst.copy_from_slice(src);

        Ok(())
    }

    /// Compute dirty blocks and bitmap. It does not check if the memory range
    /// is valid.
    fn update_dirty(&mut self, addr: VirtAddr, size: usize) {
        let block_start = *addr / DIRTY_BLOCK_SIZE;
        // Calculate the start of the next block. It takes into account corner
        // cases like `end` being equal to the start of the next block.
        let block_end =
            (*addr + size + (DIRTY_BLOCK_SIZE - 1)) / DIRTY_BLOCK_SIZE;

        for block in block_start..block_end {
            let idx = block / 64;
            let bit = block % 64;

            if self.dirty_bitmap[idx] & (1 << bit) == 0 {
                self.dirty_bitmap[idx] |= 1 << bit;
                self.dirty.push(block);
            }
        }
    }

    /// Write an integer value into a given memory address. This function will
    /// fail if the destination memory is not writable.
    pub fn write_int<T: LeBytes>(
        &mut self,
        addr: VirtAddr,
        value: T::Target,
    ) -> Result<(), Error> {
        let bytes = T::to_le_bytes(value);
        self.write(addr, &bytes[..mem::size_of::<T::Target>()])?;
        Ok(())
    }

    /// Read the data starting at the specified memory address into an integer.
    /// This function will fail if the source memory is not readable.
    pub fn read_int<T: LeBytes>(
        &self,
        addr: VirtAddr,
    ) -> Result<T::Target, Error> {
        let mut bytes = [0u8; 16];
        self.read(addr, &mut bytes[..mem::size_of::<T::Target>()])?;
        Ok(T::from_le_bytes(bytes))
    }

    /// Write an integer value into a given memory address. This function will
    /// This function does not check memory permissions.
    pub fn poke_int<T: LeBytes>(
        &mut self,
        addr: VirtAddr,
        value: T::Target,
    ) -> Result<(), Error> {
        let bytes = T::to_le_bytes(value);
        self.poke(addr, &bytes[..mem::size_of::<T::Target>()])?;
        Ok(())
    }

    /// Read the data starting at the specified memory address into an integer.
    /// This function does not check memory permissions.
    pub fn peek_int<T: LeBytes>(
        &self,
        addr: VirtAddr,
    ) -> Result<T::Target, Error> {
        let mut bytes = [0u8; 16];
        self.peek(addr, &mut bytes[..mem::size_of::<T::Target>()])?;
        Ok(T::from_le_bytes(bytes))
    }
}

/// Types implementing this trait can be converted to and from little-endian
/// bytes.
pub trait LeBytes {
    type Target;

    /// Convert an array of bytes into a value of the associated type.
    fn from_le_bytes(bytes: [u8; 16]) -> Self::Target;

    /// Convert a value of the associated type into an array of bytes.
    fn to_le_bytes(value: Self::Target) -> [u8; 16];
}

macro_rules! impl_le_bytes {
    ($Ty: ty) => {
        impl LeBytes for $Ty {
            type Target = $Ty;

            fn from_le_bytes(bytes: [u8; 16]) -> $Ty {
                let src = &bytes[..mem::size_of::<$Ty>()];

                <$Ty>::from_le_bytes(src.try_into().unwrap())
            }

            fn to_le_bytes(value: $Ty) -> [u8; 16] {
                let bytes = value.to_le_bytes();

                let mut result = [0u8; 16];
                let dst = &mut result[..mem::size_of::<$Ty>()];
                dst.copy_from_slice(&bytes);

                result
            }
        }
    };
}

// Implemente LeBytes for unsigned integers.
impl_le_bytes!(u8);
impl_le_bytes!(u16);
impl_le_bytes!(u32);
impl_le_bytes!(u64);
impl_le_bytes!(u128);

// Implement LeBytes for signed integers.
impl_le_bytes!(i8);
impl_le_bytes!(i16);
impl_le_bytes!(i32);
impl_le_bytes!(i64);
impl_le_bytes!(i128);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmu_new() {
        let mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        let want = Mmu {
            size: DIRTY_BLOCK_SIZE,
            memory: vec![0; DIRTY_BLOCK_SIZE],
            perms: vec![Perm(0); DIRTY_BLOCK_SIZE],
            dirty: vec![],
            dirty_bitmap: vec![0; 1],
        };

        assert_eq!(mmu, want);
    }

    #[test]
    #[should_panic]
    fn mmu_new_small_size() {
        Mmu::new(DIRTY_BLOCK_SIZE - 1);
    }

    #[test]
    fn mmu_check_perms() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        mmu.set_perms(VirtAddr(0), 8, Perm(PERM_WRITE | PERM_READ))
            .unwrap();
        mmu.check_perms(VirtAddr(0), 8, Perm(PERM_WRITE | PERM_READ))
            .unwrap();
    }

    #[test]
    #[should_panic]
    fn mmu_check_perms_subset() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        mmu.set_perms(VirtAddr(0), 8, Perm(PERM_WRITE)).unwrap();

        mmu.check_perms(VirtAddr(0), 8, Perm(PERM_WRITE | PERM_READ))
            .unwrap();
    }

    #[test]
    fn mmu_check_perms_oob() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        match mmu.set_perms(
            VirtAddr(DIRTY_BLOCK_SIZE + 5),
            16,
            Perm(PERM_WRITE),
        ) {
            Err(Error::InvalidAddress { .. }) => return,
            Err(err) => panic!("Wrong error {:?}", err),
            _ => panic!("The function didn't return an error"),
        }
    }

    #[test]
    fn mmu_check_perms_integer_overflow() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        match mmu.set_perms(VirtAddr(usize::MAX), 1, Perm(PERM_WRITE)) {
            Err(Error::AddressIntegerOverflow { .. }) => return,
            Err(err) => panic!("Wrong error {:?}", err),
            _ => panic!("The function didn't return an error"),
        }
    }

    #[test]
    fn mmu_poke_peek() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        mmu.poke(VirtAddr(0), &[1, 2, 3, 4]).unwrap();

        let mut got = [0u8; 4];
        mmu.peek(VirtAddr(0), &mut got).unwrap();

        assert_eq!(&got, &[1, 2, 3, 4]);
    }

    #[test]
    fn mmu_write_read() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);

        mmu.set_perms(VirtAddr(0), 4, Perm(PERM_READ | PERM_WRITE))
            .unwrap();
        mmu.write(VirtAddr(0), &[1, 2, 3, 4]).unwrap();

        let mut got = [0u8; 4];
        mmu.read(VirtAddr(0), &mut got).unwrap();

        assert_eq!(&got, &[1, 2, 3, 4]);
    }

    #[test]
    fn mmu_write_not_allowed() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        match mmu.write(VirtAddr(0), &[1, 2, 3, 4]) {
            Err(Error::NotAllowed { .. }) => return,
            Err(err) => panic!("Wrong error {:?}", err),
            _ => panic!("The function didn't return an error"),
        }
    }

    #[test]
    fn mmu_read_not_allowed() {
        let mmu = Mmu::new(DIRTY_BLOCK_SIZE);

        let mut tmp = [0u8; 2];
        match mmu.read(VirtAddr(0), &mut tmp) {
            Err(Error::NotAllowed { .. }) => return,
            Err(err) => panic!("Wrong error {:?}", err),
            _ => panic!("The function didn't return an error"),
        }
    }

    #[test]
    fn mmu_raw_after_write() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        mmu.set_perms(VirtAddr(0), 3, Perm(PERM_WRITE | PERM_RAW))
            .unwrap();
        mmu.write(VirtAddr(0), &[1, 2]).unwrap();

        assert_eq!(&mmu.memory[..4], &[1, 2, 0, 0]);
        assert_eq!(
            &mmu.perms[..4],
            &[
                Perm(PERM_WRITE | PERM_READ),
                Perm(PERM_WRITE | PERM_READ),
                Perm(PERM_WRITE | PERM_RAW),
                Perm(0)
            ]
        );
    }

    #[test]
    fn mmu_raw_ok() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        mmu.set_perms(VirtAddr(0), 2, Perm(PERM_READ | PERM_WRITE))
            .unwrap();
        mmu.set_perms(VirtAddr(2), 2, Perm(PERM_WRITE | PERM_RAW))
            .unwrap();
        mmu.write(VirtAddr(0), &[1, 2, 3, 4]).unwrap();

        let mut got = [0u8; 4];
        mmu.read(VirtAddr(0), &mut got).unwrap();

        assert_eq!(&got, &[1, 2, 3, 4]);
    }

    #[test]
    fn mmu_raw_unitialized() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        mmu.set_perms(VirtAddr(0), 2, Perm(PERM_READ)).unwrap();
        mmu.set_perms(VirtAddr(2), 2, Perm(PERM_WRITE | PERM_RAW))
            .unwrap();

        let mut tmp = [0u8; 2];
        match mmu.read(VirtAddr(1), &mut tmp) {
            Err(Error::UnitializedMemory { .. }) => return,
            Err(err) => panic!("Wrong error {:?}", err),
            _ => panic!("The function didn't return an error"),
        }
    }

    #[test]
    fn mmu_raw_not_allowed() {
        let mut mmu = Mmu::new(DIRTY_BLOCK_SIZE);
        mmu.set_perms(VirtAddr(0), 2, Perm(PERM_WRITE)).unwrap();
        mmu.set_perms(VirtAddr(2), 2, Perm(PERM_WRITE | PERM_RAW))
            .unwrap();

        let mut tmp = [0u8; 2];
        match mmu.read(VirtAddr(1), &mut tmp) {
            Err(Error::NotAllowed { .. }) => return,
            Err(err) => panic!("Wrong error {:?}", err),
            _ => panic!("The function didn't return an error"),
        }
    }

    #[test]
    fn mmu_reset() {
        let mmu = Mmu::new(1024 * DIRTY_BLOCK_SIZE);
        let mut mmu_fork = mmu.fork();

        mmu_fork
            .set_perms(VirtAddr(DIRTY_BLOCK_SIZE + 4), 4, Perm(PERM_WRITE))
            .unwrap();
        mmu_fork
            .write(VirtAddr(DIRTY_BLOCK_SIZE + 4), &[1, 2, 3, 4])
            .unwrap();

        let mut got = [0u8; 4];

        mmu_fork
            .peek(VirtAddr(DIRTY_BLOCK_SIZE + 4), &mut got)
            .unwrap();
        assert_eq!(&got, &[1, 2, 3, 4]);

        mmu_fork.reset(&mmu);

        mmu_fork
            .peek(VirtAddr(DIRTY_BLOCK_SIZE + 4), &mut got)
            .unwrap();
        assert_eq!(&got, &[0, 0, 0, 0]);
    }

    #[test]
    fn mmu_reset_two_blocks() {
        let mmu = Mmu::new(1024 * DIRTY_BLOCK_SIZE);
        let mut mmu_fork = mmu.fork();

        mmu_fork
            .set_perms(VirtAddr(DIRTY_BLOCK_SIZE - 2), 4, Perm(PERM_WRITE))
            .unwrap();
        mmu_fork
            .write(VirtAddr(DIRTY_BLOCK_SIZE - 2), &[1, 2, 3, 4])
            .unwrap();

        let mut got = [0u8; 4];

        mmu_fork
            .peek(VirtAddr(DIRTY_BLOCK_SIZE - 2), &mut got)
            .unwrap();
        assert_eq!(&got, &[1, 2, 3, 4]);

        mmu_fork.reset(&mmu);

        mmu_fork
            .peek(VirtAddr(DIRTY_BLOCK_SIZE - 2), &mut got)
            .unwrap();
        assert_eq!(&got, &[0, 0, 0, 0]);
    }

    #[test]
    fn mmu_reset_one_of_two_blocks() {
        let mmu = Mmu::new(1024 * DIRTY_BLOCK_SIZE);
        let mut mmu_fork = mmu.fork();

        mmu_fork
            .poke(VirtAddr(DIRTY_BLOCK_SIZE - 2), &[1, 2, 3, 4])
            .unwrap();
        mmu_fork
            .set_perms(VirtAddr(1024), 2, Perm(PERM_WRITE))
            .unwrap();

        let mut got = [0u8; 4];

        mmu_fork
            .peek(VirtAddr(DIRTY_BLOCK_SIZE - 2), &mut got)
            .unwrap();
        assert_eq!(&got, &[1, 2, 3, 4]);

        mmu_fork.reset(&mmu);

        mmu_fork
            .peek(VirtAddr(DIRTY_BLOCK_SIZE - 2), &mut got)
            .unwrap();
        assert_eq!(&got, &[1, 2, 0, 0]);
    }

    #[test]
    fn mmu_reset_all() {
        let mmu = Mmu::new(1024 * DIRTY_BLOCK_SIZE);
        let mut mmu_fork = mmu.fork();

        mmu_fork
            .set_perms(
                VirtAddr(0),
                1024 * DIRTY_BLOCK_SIZE,
                Perm(PERM_WRITE | PERM_RAW),
            )
            .unwrap();
        mmu_fork
            .write(VirtAddr(DIRTY_BLOCK_SIZE + 4), &[1, 2, 3, 4])
            .unwrap();

        let mut got = [0u8; 4];

        mmu_fork
            .read(VirtAddr(DIRTY_BLOCK_SIZE + 4), &mut got)
            .unwrap();
        assert_eq!(&got, &[1, 2, 3, 4]);

        mmu_fork.reset(&mmu);

        mmu_fork.peek(VirtAddr(4), &mut got).unwrap();
        assert_eq!(&got, &[0, 0, 0, 0]);
    }

    #[test]
    fn mmu_write_read_int() {
        let mut mmu_init = Mmu::new(DIRTY_BLOCK_SIZE);

        mmu_init
            .set_perms(
                VirtAddr(0),
                DIRTY_BLOCK_SIZE,
                Perm(PERM_READ | PERM_WRITE),
            )
            .unwrap();

        let mut mmu = mmu_init.fork();

        const VAL_U8: u8 = 0x11;
        const VAL_U16: u16 = 0x1122;
        const VAL_U32: u32 = 0x11223344;
        const VAL_U64: u64 = 0x1122334455667788;
        const VAL_U128: u128 = 0x11223344556677881122334455667788;

        // u8
        mmu.write_int::<u8>(VirtAddr(0), VAL_U8).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U8 as u128);

        // u16
        mmu.reset(&mmu_init);
        mmu.write_int::<u16>(VirtAddr(0), VAL_U16).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U16 as u128);

        // u32
        mmu.reset(&mmu_init);
        mmu.write_int::<u32>(VirtAddr(0), VAL_U32).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U32 as u128);

        // u64
        mmu.reset(&mmu_init);
        mmu.write_int::<u64>(VirtAddr(0), VAL_U64).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U64 as u128);

        // u128
        mmu.reset(&mmu_init);
        mmu.write_int::<u128>(VirtAddr(0), VAL_U128).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U128 as u128);
    }

    #[test]
    fn mmu_poke_peek_int() {
        let mut mmu_init = Mmu::new(DIRTY_BLOCK_SIZE);

        mmu_init
            .set_perms(
                VirtAddr(0),
                DIRTY_BLOCK_SIZE,
                Perm(PERM_READ | PERM_WRITE),
            )
            .unwrap();

        let mut mmu = mmu_init.fork();

        const VAL_U8: u8 = 0x11;
        const VAL_U16: u16 = 0x1122;
        const VAL_U32: u32 = 0x11223344;
        const VAL_U64: u64 = 0x1122334455667788;
        const VAL_U128: u128 = 0x11223344556677881122334455667788;

        // u8
        mmu.poke_int::<u8>(VirtAddr(0), VAL_U8).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U8 as u128);

        // u16
        mmu.reset(&mmu_init);
        mmu.poke_int::<u16>(VirtAddr(0), VAL_U16).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U16 as u128);

        // u32
        mmu.reset(&mmu_init);
        mmu.poke_int::<u32>(VirtAddr(0), VAL_U32).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U32 as u128);

        // u64
        mmu.reset(&mmu_init);
        mmu.poke_int::<u64>(VirtAddr(0), VAL_U64).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U64 as u128);

        // u128
        mmu.reset(&mmu_init);
        mmu.poke_int::<u128>(VirtAddr(0), VAL_U128).unwrap();
        let got = mmu.peek_int::<u128>(VirtAddr(0)).unwrap();
        assert_eq!(got, VAL_U128 as u128);
    }
}
