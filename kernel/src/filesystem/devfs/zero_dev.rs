use crate::filesystem::vfs::make_rawdev;
use crate::filesystem::vfs::{
    core::generate_inode_id, FilePrivateData, FileSystem, FileType, IndexNode, Metadata, PollStatus,
};
use crate::{
    include::bindings::bindings::{EINVAL, ENOTSUP},
    libs::spinlock::SpinLock,
    time::TimeSpec,
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
// use uuid::{uuid, Uuid};
use super::{DevFS, DeviceINode};

#[derive(Debug)]
pub struct ZeroInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedZeroInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
}

#[derive(Debug)]
pub struct LockedZeroInode(SpinLock<ZeroInode>);

impl LockedZeroInode {
    pub fn new() -> Arc<Self> {
        let inode = ZeroInode {
            // uuid: Uuid::new_v5(),
            self_ref: Weak::default(),
            fs: Weak::default(),
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::CharDevice, // 文件夹，block设备，char设备
                mode: 0o666,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: make_rawdev(1, 3), // 这里用来作为device number
            },
        };

        let result = Arc::new(LockedZeroInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedZeroInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }
}

impl IndexNode for LockedZeroInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(&self, _data: &mut FilePrivateData) -> Result<(), i32> {
        Err(-(ENOTSUP as i32))
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), i32> {
        Err(-(ENOTSUP as i32))
    }

    fn metadata(&self) -> Result<Metadata, i32> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn list(&self) -> Result<Vec<String>, i32> {
        Err(-(ENOTSUP as i32))
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), i32> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn poll(&self) -> Result<PollStatus, i32> {
        return Ok(PollStatus {
            flags: PollStatus::READ_MASK | PollStatus::WRITE_MASK,
        });
    }

    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, i32> {
        if buf.len() < len {
            return Err(-(EINVAL as i32));
        }

        for i in 0..len {
            buf[i] = 0;
        }

        return Ok(len);
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, i32> {
        if buf.len() < len {
            return Err(-(EINVAL as i32));
        }

        Ok(len)
    }
}
