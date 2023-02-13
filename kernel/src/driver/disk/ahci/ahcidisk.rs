use super::{
    hba::{HbaCmdTable, HbaPrdtEntry},
    virt_2_phys,
};
use crate::include::bindings::bindings::{EOVERFLOW, E_NOEMPTYSLOT, E_PORT_HUNG};
use crate::io::{device::BlockDevice, disk_info::Partition, SeekFrom};
use crate::libs::{spinlock::SpinLock, vec_cursor::VecCursor};
use crate::{
    driver::disk::ahci::{
        hba::{
            FisRegH2D, FisType, HbaCmdHeader, HbaPort, ATA_CMD_READ_DMA_EXT, ATA_DEV_BUSY,
            ATA_DEV_DRQ,
        },
        phys_2_virt,
    },
    kerror,
};
use crate::{filesystem::mbr::MbrDiskPartionTable, libs::spinlock::SpinLockGuard};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt::Debug;
use core::ops::{Deref, DerefMut};
use core::{mem::size_of, ptr::write_bytes};

/// @brief: 只支持MBR分区格式的磁盘结构体
pub struct AhciDisk {
    pub name: String,
    pub flags: u16,                  // 磁盘的状态flags
    pub part_s: Vec<Arc<Partition>>, // 磁盘分区数组
    // port: &'static mut HbaPort,      // 控制硬盘的端口
    pub ctrl_num: u8,
    pub port_num: u8,
}

/// @brief: 带锁的AhciDisk
#[derive(Debug)]
pub struct LockedAhciDisk(pub SpinLock<AhciDisk>);

/// 函数实现
impl Debug for AhciDisk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{{ name: {}, flags: {}, part_s: {:?} }}",
            self.name, self.flags, self.part_s
        );
        return Ok(());
    }
}

impl BlockDevice for AhciDisk {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn blk_size_log2(&self) -> u8 {
        9
    }

    fn read_at(
        &self,
        lba_id_start: crate::io::device::BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        if count * 512 > buf.len() {
            // 不可能的操作
            return Err(-(EOVERFLOW as i32));
        } else if count == 0 {
            return Ok(0);
        }

        v_write!(self.port.is, u32::MAX); // Clear pending interrupt bits

        let slot = self.port.find_cmdslot().unwrap_or(u32::MAX);
        if slot == u32::MAX {
            return Err(-(E_NOEMPTYSLOT as i32));
        }

        let cmdheader: &mut HbaCmdHeader = unsafe {
            &mut *(phys_2_virt(
                v_read!(self.port.clb) as usize
                    + slot as usize * size_of::<HbaCmdHeader>() as usize,
            ) as *mut HbaCmdHeader)
        };

        // write_volatile(dst, src);

        v_write_bit!(
            cmdheader.cfl,
            (1 << 5) - 1 as u8,
            (size_of::<FisRegH2D>() / size_of::<u32>()) as u8
        ); // Command FIS size

        v_set_bit!(cmdheader.cfl, 1 << 6, false); //  Read/Write bit : Read from device
        v_write!(cmdheader.prdtl, ((count - 1) >> 4 + 1) as u16); // PRDT entries count

        // 设置数据存放地址
        let mut buf_ptr = buf as *mut [u8] as *mut usize as usize;
        let cmdtbl =
            &mut unsafe { *(phys_2_virt(v_read!(cmdheader.ctba) as usize) as *mut HbaCmdTable) };
        let mut tmp_count = count;
        unsafe {
            // 清空整个table的旧数据
            write_bytes(
                cmdtbl,
                0,
                (size_of::<HbaCmdTable>()
                    + (v_read!(cmdheader.prdtl) - 1) as usize * size_of::<HbaPrdtEntry>())
                    as usize,
            );
        }

        // 8K bytes (16 sectors) per PRDT
        for i in 0..((v_read!(cmdheader.prdtl) - 1) as usize) {
            v_write!(cmdtbl.prdt_entry[i].dba, virt_2_phys(buf_ptr) as u64);
            v_write_bit!(cmdtbl.prdt_entry[i].dbc, (1 << 22) - 1, 8 * 1024 - 1); // 数据长度
            v_set_bit!(cmdtbl.prdt_entry[i].dbc, 1 << 31, true); // 允许中断
            buf_ptr += 4 * 1024;
            tmp_count -= 16;
        }

        // Last entry
        let las = (v_read!(cmdheader.prdtl) - 1) as usize;
        v_write!(cmdtbl.prdt_entry[las].dba, virt_2_phys(buf_ptr) as u64);
        v_write_bit!(
            cmdtbl.prdt_entry[las].dbc,
            (1 << 22) - 1,
            ((tmp_count << 9) - 1) as u32
        ); // 数据长度
        v_set_bit!(cmdtbl.prdt_entry[las].dbc, 1 << 31, true); // 允许中断

        // 设置命令
        let cmdfis =
            &mut unsafe { *((&mut cmdtbl.cfis) as *mut [u8] as *mut usize as *mut FisRegH2D) };
        v_write!(cmdfis.fis_type, FisType::RegH2D as u8);
        v_set_bit!(cmdfis.pm, 1 << 7, true); // command_bit set
        v_write!(cmdfis.command, ATA_CMD_READ_DMA_EXT);

        v_write!(cmdfis.lba0, lba_id_start as u8);
        v_write!(cmdfis.lba1, (lba_id_start >> 8) as u8);
        v_write!(cmdfis.lba2, (lba_id_start >> 16) as u8);
        v_write!(cmdfis.lba3, (lba_id_start >> 24) as u8);
        v_write!(cmdfis.lba4, (lba_id_start >> 32) as u8);
        v_write!(cmdfis.lba5, (lba_id_start >> 40) as u8);

        v_write!(cmdfis.countl, (count & 0xFF) as u8);
        v_write!(cmdfis.counth, ((count >> 8) & 0xFF) as u8);

        v_write!(cmdfis.device, 1 << 6); // LBA Mode

        // 等待之前的操作完成
        let mut spin_count = 0;
        let SPIN_LIMIT = 1000000;
        while (v_read!(self.port.tfd) as u8 & (ATA_DEV_BUSY | ATA_DEV_DRQ)) > 0
            && spin_count < SPIN_LIMIT
        {
            spin_count += 1;
        }

        if spin_count == SPIN_LIMIT {
            kerror!("Port is hung");
            return Err(-(E_PORT_HUNG as i32));
        }

        v_set_bit!(self.port.ci, 1 << slot, true); // Issue command

        // successfully read
        Ok(count * 512)
    }

    fn write_at(
        &self,
        lba_id_start: crate::io::device::BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, i32> {
        if count * 512 > buf.len() {
            // 不可能的操作
            return Err(-(EOVERFLOW as i32));
        } else if count == 0 {
            return Ok(0);
        }

        v_write!(self.port.is, u32::MAX); // Clear pending interrupt bits

        let slot = self.port.find_cmdslot().unwrap_or(u32::MAX);
        if slot == u32::MAX {
            return Err(-(E_NOEMPTYSLOT as i32));
        }

        let cmdheader: &mut HbaCmdHeader = unsafe {
            &mut *(phys_2_virt(
                v_read!(self.port.clb) as usize
                    + slot as usize * size_of::<HbaCmdHeader>() as usize,
            ) as *mut HbaCmdHeader)
        };

        v_write_bit!(
            cmdheader.cfl,
            (1 << 5) - 1 as u8,
            (size_of::<FisRegH2D>() / size_of::<u32>()) as u8
        ); // Command FIS size

        v_set_bit!(cmdheader.cfl, 7 << 5, true); // (p,c,w)都设置为1, Read/Write bit :  Write from device
        v_write!(cmdheader.prdtl, ((count - 1) >> 4 + 1) as u16); // PRDT entries count

        // 设置数据存放地址
        let mut buf_ptr = buf as *const [u8] as *mut usize as usize;
        let cmdtbl =
            &mut unsafe { *(phys_2_virt(v_read!(cmdheader.ctba) as usize) as *mut HbaCmdTable) };
        let mut tmp_count = count;
        unsafe {
            // 清空整个table的旧数据
            write_bytes(
                cmdtbl,
                0,
                (size_of::<HbaCmdTable>()
                    + (v_read!(cmdheader.prdtl) - 1) as usize * size_of::<HbaPrdtEntry>())
                    as usize,
            );
        }

        // 8K bytes (16 sectors) per PRDT
        for i in 0..((v_read!(cmdheader.prdtl) - 1) as usize) {
            v_write!(cmdtbl.prdt_entry[i].dba, virt_2_phys(buf_ptr) as u64);
            v_write_bit!(cmdtbl.prdt_entry[i].dbc, (1 << 22) - 1, 8 * 1024 - 1); // 数据长度
            v_set_bit!(cmdtbl.prdt_entry[i].dbc, 1 << 31, true); // 允许中断
            buf_ptr += 4 * 1024;
            tmp_count -= 16;
        }

        // Last entry
        let las = (v_read!(cmdheader.prdtl) - 1) as usize;
        v_write!(cmdtbl.prdt_entry[las].dba, virt_2_phys(buf_ptr) as u64);
        v_set_bit!(cmdtbl.prdt_entry[las].dbc, 1 << 31, true); // 允许中断
        v_write_bit!(
            cmdtbl.prdt_entry[las].dbc,
            (1 << 22) - 1,
            ((tmp_count << 9) - 1) as u32
        ); // 数据长度

        // 设置命令
        let cmdfis =
            &mut unsafe { *((&mut cmdtbl.cfis) as *mut [u8] as *mut usize as *mut FisRegH2D) };
        v_write!(cmdfis.fis_type, FisType::RegH2D as u8);
        v_set_bit!(cmdfis.pm, 1 << 7, true); // command_bit set
        v_write!(cmdfis.command, ATA_CMD_READ_DMA_EXT);

        v_write!(cmdfis.lba0, lba_id_start as u8);
        v_write!(cmdfis.lba1, (lba_id_start >> 8) as u8);
        v_write!(cmdfis.lba2, (lba_id_start >> 16) as u8);
        v_write!(cmdfis.lba3, (lba_id_start >> 24) as u8);
        v_write!(cmdfis.lba4, (lba_id_start >> 32) as u8);
        v_write!(cmdfis.lba5, (lba_id_start >> 40) as u8);

        v_write!(cmdfis.counth, (count & 0xFF) as u8);
        v_write!(cmdfis.counth, ((count >> 8) & 0xFF) as u8);

        v_write!(cmdfis.device, 1 << 6); // LBA Mode

        // 等待之前的操作完成
        let mut spin_count = 0;
        let SPIN_LIMIT = 1000000;
        while (v_read!(self.port.tfd) as u8 & (ATA_DEV_BUSY | ATA_DEV_DRQ)) > 0
            && spin_count < SPIN_LIMIT
        {
            spin_count += 1;
        }

        if spin_count == SPIN_LIMIT {
            kerror!("Port is hung");
            return Err(-(E_PORT_HUNG as i32));
        }

        v_set_bit!(self.port.ci, 1 << slot, true); // Issue command

        // successfully read
        Ok(count * 512)
    }

    fn sync(&self) -> Result<(), i32> {
        return Err(-1);
    }
}

impl LockedAhciDisk {
    pub fn new(
        name: String,
        flags: u16,
        // port: &'static mut HbaPort,
        ctrl_num: u8,
        port_num: u8,
    ) -> Result<Arc<LockedAhciDisk>, i32> {
        let mut part_s: Vec<Arc<Partition>> = Vec::new();

        // 构建磁盘结构体
        let this = Arc::new(LockedAhciDisk(SpinLock::new(AhciDisk {
            name,
            flags,
            part_s: Default::default(),
            ctrl_num,
            port_num,
        })));

        let table = this.read_mbr_table()?;

        let weak_this = Arc::downgrade(&this); // 获取this的弱指针
        let raw_this = Arc::into_raw(this) as *mut LockedAhciDisk;

        // 求出有多少可用分区
        for i in 0..4 {
            if table.dpte[i].part_type != 0 {
                part_s.push(Partition::new(
                    table.dpte[i].starting_sector() as u64,
                    table.dpte[i].starting_lba as u64,
                    table.dpte[i].total_sectors as u64,
                    weak_this.clone(),
                    i as u16,
                ));
            }
        }

        unsafe {
            (*raw_this).0.lock().part_s = part_s;
            return Ok(Arc::from_raw(raw_this));
        }
    }
    /// @brief: 从磁盘中读取 MBR 分区表结构体 TODO: Cursor
    pub fn read_mbr_table(&self) -> Result<MbrDiskPartionTable, i32> {
        let mut table: MbrDiskPartionTable = Default::default();

        // 数据缓冲区
        let mut buf: Vec<u8> = Vec::new();
        buf.resize(size_of::<MbrDiskPartionTable>(), 0);

        self.read_at(0, 1, &mut buf);

        // 创建 Cursor 用于按字节读取
        let mut cursor = VecCursor::new(buf);
        cursor.seek(SeekFrom::SeekCurrent(446));

        for i in 0..4 {
            table.dpte[i].flags = cursor.read_u8()?;
            table.dpte[i].starting_head = cursor.read_u8()?;
            table.dpte[i].starting_sector_cylinder = cursor.read_u16()?;
            table.dpte[i].part_type = cursor.read_u8()?;
            table.dpte[i].ending_head = cursor.read_u8()?;
            table.dpte[i].ending_sector_cylingder = cursor.read_u16()?;
            table.dpte[i].starting_lba = cursor.read_u32()?;
            table.dpte[i].total_sectors = cursor.read_u32()?;
        }
        table.bs_trailsig = cursor.read_u16()?;

        Ok(table)
    }
}

impl BlockDevice for LockedAhciDisk {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn blk_size_log2(&self) -> u8 {
        9
    }

    fn read_at(
        &self,
        lba_id_start: crate::io::device::BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        self.0.lock().read_at(lba_id_start, count, buf)
    }

    fn write_at(
        &self,
        lba_id_start: crate::io::device::BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, i32> {
        self.0.lock().write_at(lba_id_start, count, buf)
    }

    fn sync(&self) -> Result<(), i32> {
        self.0.lock().sync()
    }
}

impl Deref for LockedAhciDisk {
    type Target = AhciDisk;

    fn deref(&self) -> &Self::Target {
        return &self.0.lock();
    }
}

impl DerefMut for LockedAhciDisk {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return &mut self.0.lock();
    }
}
