use core::ffi::c_void;
use std::boxed::Box;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::ops::Deref as _;
use std::os::raw::c_ulong;
use std::ptr::NonNull;
use std::slice;
use std::time::Duration;

use crate::libbpf_sys;
use crate::util;
use crate::Error;
use crate::Map;
use crate::MapType;
use crate::Result;

type Cb<'a> = Box<dyn FnMut(&[u8]) -> i32 + 'a>;

struct RingBufferCallback<'a> {
    cb: Cb<'a>,
}

impl<'a> RingBufferCallback<'a> {
    fn new<F>(cb: F) -> Self
    where
        F: FnMut(&[u8]) -> i32 + 'a,
    {
        RingBufferCallback { cb: Box::new(cb) }
    }
}

impl Debug for RingBufferCallback<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let Self { cb } = self;
        f.debug_struct("RingBufferCallback")
            .field("cb", &(cb.deref() as *const _))
            .finish()
    }
}

/// Builds [`RingBuffer`] instances.
///
/// `ringbuf`s are a special kind of [`Map`], used to transfer data between
/// [`Program`][crate::Program]s and userspace. As of Linux 5.8, the `ringbuf`
/// map is now preferred over the `perf buffer`.
#[derive(Debug, Default)]
pub struct RingBufferBuilder<'a> {
    fd_callbacks: Vec<(i32, RingBufferCallback<'a>)>,
}

impl<'a> RingBufferBuilder<'a> {
    /// Create a new `RingBufferBuilder` object.
    pub fn new() -> Self {
        RingBufferBuilder {
            fd_callbacks: vec![],
        }
    }

    /// Add a new ringbuf `map` and associated `callback` to this ring buffer
    /// manager. The callback should take one argument, a slice of raw bytes,
    /// and return an i32.
    ///
    /// Non-zero return values in the callback will stop ring buffer consumption early.
    ///
    /// The callback provides a raw byte slice. You may find libraries such as
    /// [`plain`](https://crates.io/crates/plain) helpful.
    pub fn add<NewF>(&mut self, map: &Map, callback: NewF) -> Result<&mut Self>
    where
        NewF: FnMut(&[u8]) -> i32 + 'a,
    {
        if map.map_type() != MapType::RingBuf {
            return Err(Error::InvalidInput("Must use a RingBuf map".into()));
        }
        self.fd_callbacks
            .push((map.fd(), RingBufferCallback::new(callback)));
        Ok(self)
    }

    /// Build a new [`RingBuffer`]. Must have added at least one ringbuf.
    pub fn build(self) -> Result<RingBuffer<'a>> {
        let mut cbs = vec![];
        let mut ptr: Option<NonNull<libbpf_sys::ring_buffer>> = None;
        let c_sample_cb: libbpf_sys::ring_buffer_sample_fn = Some(Self::call_sample_cb);

        for (fd, callback) in self.fd_callbacks {
            let sample_cb_ptr = Box::into_raw(Box::new(callback));
            match ptr {
                None => {
                    // Allocate a new ringbuf manager and add a ringbuf to it
                    ptr = Some(util::create_bpf_entity_checked(|| unsafe {
                        libbpf_sys::ring_buffer__new(
                            fd,
                            c_sample_cb,
                            sample_cb_ptr as *mut _,
                            std::ptr::null_mut(),
                        )
                    })?);
                }
                Some(ptr) => {
                    // Add a ringbuf to the existing ringbuf manager
                    let err = unsafe {
                        libbpf_sys::ring_buffer__add(
                            ptr.as_ptr(),
                            fd,
                            c_sample_cb,
                            sample_cb_ptr as *mut _,
                        )
                    };

                    // Handle errors
                    if err != 0 {
                        return Err(Error::System(err));
                    }
                }
            }

            unsafe { cbs.push(Box::from_raw(sample_cb_ptr)) };
        }

        match ptr {
            Some(ptr) => Ok(RingBuffer { ptr, _cbs: cbs }),
            None => Err(Error::InvalidInput(
                "You must add at least one ring buffer map and callback before building".into(),
            )),
        }
    }

    unsafe extern "C" fn call_sample_cb(ctx: *mut c_void, data: *mut c_void, size: c_ulong) -> i32 {
        let callback_struct = ctx as *mut RingBufferCallback;
        let callback = unsafe { (*callback_struct).cb.as_mut() };
        let slice = unsafe { slice::from_raw_parts(data as *const u8, size as usize) };

        callback(slice)
    }
}

/// The canonical interface for managing a collection of `ringbuf` maps.
///
/// `ringbuf`s are a special kind of [`Map`], used to transfer data between
/// [`Program`][crate::Program]s and userspace. As of Linux 5.8, the `ringbuf`
/// map is now preferred over the `perf buffer`.
#[derive(Debug)]
pub struct RingBuffer<'a> {
    ptr: NonNull<libbpf_sys::ring_buffer>,
    #[allow(clippy::vec_box)]
    _cbs: Vec<Box<RingBufferCallback<'a>>>,
}

impl<'a> RingBuffer<'a> {
    /// Poll from all open ring buffers, calling the registered callback for
    /// each one. Polls continually until we either run out of events to consume
    /// or `timeout` is reached. If `timeout` is Duration::MAX, this will block
    /// indefinitely until an event occurs.
    pub fn poll(&self, timeout: Duration) -> Result<()> {
        let mut timeout_ms = -1;
        if timeout != Duration::MAX {
            timeout_ms = timeout.as_millis() as i32;
        }

        let ret = unsafe { libbpf_sys::ring_buffer__poll(self.ptr.as_ptr(), timeout_ms) };

        util::parse_ret(ret)
    }

    /// Greedily consume from all open ring buffers, calling the registered
    /// callback for each one. Consumes continually until we run out of events
    /// to consume or one of the callbacks returns a non-zero integer.
    pub fn consume(&self) -> Result<()> {
        let ret = unsafe { libbpf_sys::ring_buffer__consume(self.ptr.as_ptr()) };

        util::parse_ret(ret)
    }

    /// Get an fd that can be used to sleep until data is available
    pub fn epoll_fd(&self) -> i32 {
        unsafe { libbpf_sys::ring_buffer__epoll_fd(self.ptr.as_ptr()) }
    }
}

// SAFETY: `ring_buffer` objects can safely be polled from any thread.
unsafe impl Send for RingBuffer<'_> {}

impl Drop for RingBuffer<'_> {
    fn drop(&mut self) {
        unsafe {
            libbpf_sys::ring_buffer__free(self.ptr.as_ptr());
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Check that `RingBuffer` is `Send`.
    #[test]
    fn perfbuffer_is_send() {
        fn test<T>()
        where
            T: Send,
        {
        }

        test::<RingBuffer>();
    }
}
