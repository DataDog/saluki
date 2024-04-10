use bytes::{buf::UninitSlice, Buf, BufMut};

use saluki_core::buffers::{buffered_newtype, Clearable};

use super::IoBuffer;

pub struct FixedSizeVec {
    start_idx: usize,
    read_idx: usize,
    data: Vec<u8>,
}

impl FixedSizeVec {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            start_idx: 0,
            read_idx: 0,
            data: Vec::with_capacity(capacity),
        }
    }
}

impl Clearable for FixedSizeVec {
    fn clear(&mut self) {
        self.start_idx = 0;
        self.read_idx = 0;
        self.data.clear();
    }
}

buffered_newtype! {
    outer => BytesBuffer,
    inner => FixedSizeVec,
    clear => |this| this.clear()
}

impl Buf for BytesBuffer {
    fn remaining(&self) -> usize {
        self.data().read_idx - self.data().start_idx
    }

    fn chunk(&self) -> &[u8] {
        let data = self.data();
        &data.data[data.start_idx..data.read_idx]
    }

    fn advance(&mut self, cnt: usize) {
        let data = self.data_mut();
        assert!(data.start_idx + cnt <= data.read_idx);
        data.start_idx += cnt;
    }
}

unsafe impl BufMut for BytesBuffer {
    fn remaining_mut(&self) -> usize {
        let data = self.data();
        data.data.capacity() - data.data.len()
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        self.data_mut().data.spare_capacity_mut().into()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        let new_len = self.data().data.len() + cnt;
        self.data_mut().data.set_len(new_len);
        self.data_mut().read_idx += cnt;
    }
}

impl IoBuffer for BytesBuffer {}
