use std::cell::Cell;
use std::io::{self, Read, Write};

pub const BUFFER_CAPACITY: usize = 32 * 1024;

/// A buffer that contains 2N bytes, which allows a writer to write N bytes at a
/// time, and a reader to read N bytes at a time.
///
/// Unlike a ring buffer, all readable and writable sections of the buffer are a
/// continuous segment of memory. This allows slices of the buffer to be used
/// directly instead of maintaining a separate buffer and copying sections of
/// the ring buffer to and from it.
///
/// Note that *write* and *read* are named from the buffer's perspective, so the
/// read methods act on the write page and the write methods act on the read page.
pub struct FlipBuffer {
    read_page: Cell<Box<[u8]>>,
    write_page: Cell<Box<[u8]>>,
    read_cursor: usize,
    read_max: usize,
    write_cursor: usize,
}

/// The result of an operation that copies data from a buffer to an IO stream,
/// or an IO stream to a buffer.
///
/// Compared to an [`io::Result`], this conveys more information for
/// [`FlipBuffer::write_to`] and similar methods that bundle together two
/// operations. These operations could fail because the buffer is full/empty, or
/// because the underlying IO stream cannot accept/produce bytes at the moment
/// we're reading it.
#[derive(Debug, PartialEq)]
pub enum CopyResult {
    BufferNotReady,
    IOCompleted(usize),
    IOError(io::ErrorKind),
}

/// How an operation affected the state of the buffer. This is intended for
/// optimizing polling when transferring the buffer to a [`Write`] instance.
/// If the buffer is empty then we don't have to listen for write events.
///
/// There is no "became full" / "became non-full" change because we always
/// have to listen for incoming data on a [`Read`] instance. Even if we
/// cannot accept the data we have to drain the reader to keep it from
/// blocking.
#[derive(Debug, PartialEq)]
pub enum BufferStateChange {
    BecameEmpty,
    BecameNonEmpty,
    None,
}

/// Converts an [`io::Result`] from an IO operation (returning a byte count)
/// into a [`CopyResult`]. This never returns a [`CopyResult::BufferNotReady`]
/// because the buffer must have space available for the IO operation to begin.
impl From<io::Result<usize>> for CopyResult {
    fn from(value: io::Result<usize>) -> Self {
        match value {
            Ok(size) => CopyResult::IOCompleted(size),
            Err(err) => CopyResult::IOError(err.kind()),
        }
    }
}

/// Allocates a byte array of the given size directly on the heap. This allows
/// arrays of any size to be allocated without using the stack, bypassing stack
/// overflow errors that would occur from doing this the naive way.
fn heap_buffer(size: usize) -> Box<[u8]> {
    let mut buffer = Vec::with_capacity(size);
    buffer.resize(size, 0u8);
    buffer.into_boxed_slice()
}

impl FlipBuffer {
    /// Creates a new FlipBuffer that invokes the given event handlers when its
    /// state changes.
    pub fn new() -> Self {
        FlipBuffer {
            read_page: Cell::new(heap_buffer(BUFFER_CAPACITY)),
            write_page: Cell::new(heap_buffer(BUFFER_CAPACITY)),
            read_cursor: 0,
            read_max: 0,
            write_cursor: 0,
        }
    }

    /// Swaps the role of the read buffer and the write buffer. The write buffer
    /// becomes readable, ending at the position where the writer stopped
    /// writing. The read buffer becomes writable.
    ///
    /// After this point any data in the write buffer (what used to be the read
    /// buffer) is be unreadable.
    fn flip_buffers(&mut self) {
        let written = self.write_cursor;
        self.read_cursor = 0;
        self.read_max = written;
        self.write_cursor = 0;
        self.read_page.swap(&self.write_page);
    }

    /// Whether data is available in the read page.
    fn is_read_empty(&self) -> bool {
        self.read_cursor == self.read_max
    }

    /// Whether the write page contains any data.
    fn is_write_empty(&self) -> bool {
        self.write_cursor == 0
    }

    /// Returns the total amount of data in the buffer.
    pub fn writable_size(&self) -> usize {
        (self.read_max - self.read_cursor) + self.write_cursor
    }

    /// Provides the readable portion of this buffer to a [`std::io::Read`] to
    /// read as much as possible.
    pub fn read_from<R: Read + ?Sized>(
        &mut self,
        reader: &mut R,
    ) -> (CopyResult, BufferStateChange) {
        let was_empty = self.is_read_empty() && self.is_write_empty();
        if self.write_cursor == BUFFER_CAPACITY && self.read_cursor == self.read_max {
            // Steal the read buffer if there is no data that we would clobber.
            self.flip_buffers();
        }

        let writable = BUFFER_CAPACITY - self.write_cursor;
        if writable == 0 {
            return (CopyResult::BufferNotReady, BufferStateChange::None);
        }

        let mut change = BufferStateChange::None;
        let slice = &mut self.write_page.get_mut()[self.write_cursor..BUFFER_CAPACITY];
        let result = reader
            .read(slice)
            .map(|size| {
                self.write_cursor += size;
                if size > 0 && was_empty {
                    change = BufferStateChange::BecameNonEmpty;
                }
                size
            })
            .into();
        (result, change)
    }

    /// Like [`read_from`], but if this buffer is full it drains whatever data
    /// is immediately available within the reader.
    pub fn read_or_drain<R: Read + ?Sized>(
        &mut self,
        reader: &mut R,
    ) -> (CopyResult, BufferStateChange) {
        match self.read_from(reader) {
            (CopyResult::BufferNotReady, change) => {
                let mut buffer = heap_buffer(8192);
                if let Err(err) = reader.read(&mut buffer) {
                    (CopyResult::IOError(err.kind()), change)
                } else {
                    (CopyResult::BufferNotReady, change)
                }
            }
            result => result,
        }
    }

    /// Provides the writable portion of this buffer to a [`std::io::Write`] to
    /// write as much as possible. Any bytes written will not be written again
    /// in future calls.
    pub fn write_to<W: Write + ?Sized>(
        &mut self,
        writer: &mut W,
    ) -> (CopyResult, BufferStateChange) {
        if self.read_cursor == self.read_max && self.write_cursor > 0 {
            // Steal the write buffer if it has some data we can read.
            self.flip_buffers();
        }

        let readable = self.read_max - self.read_cursor;
        if readable == 0 {
            return (CopyResult::BufferNotReady, BufferStateChange::None);
        }

        let slice = &self.read_page.get_mut()[self.read_cursor..self.read_max];
        let mut change = BufferStateChange::None;
        let result = writer
            .write(slice)
            .map(|size| {
                self.read_cursor += size;
                if self.is_read_empty() && self.is_write_empty() {
                    // Previous state does not matter here since we were not
                    // empty before. If we were BufferNotReady would have
                    // happened instead.
                    change = BufferStateChange::BecameEmpty;
                }
                size
            })
            .into();
        (result, change)
    }

    /// Ignores any data written since the last read. Note that previous reads
    /// may or may not be affected, depending on how much data they read and
    /// when the last write was.
    pub fn discard(&mut self) -> BufferStateChange {
        let direct_readable = self.is_read_empty();
        let flip_readable = self.is_write_empty();

        self.write_cursor = 0;

        if direct_readable && !flip_readable {
            BufferStateChange::BecameEmpty
        } else {
            BufferStateChange::None
        }
    }
}

#[cfg(test)]
mod test {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn input_empty_buffer() {
        let input = Vec::new();
        let mut cursor = Cursor::new(input);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (CopyResult::IOCompleted(0), BufferStateChange::None),
            buffer.read_from(&mut cursor)
        );
    }

    #[test]
    fn input_empty_read_page() {
        let input: Vec<u8> = (0..10).collect();
        let mut cursor = Cursor::new(input);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(10),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut cursor)
        );
    }

    #[test]
    fn input_with_partial_read_page() {
        let input1: Vec<u8> = (0..10).collect();
        let input2: Vec<u8> = (10..20).collect();
        let mut cursor = Cursor::new(input1);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(10),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut cursor)
        );

        cursor = Cursor::new(input2);
        assert_eq!(
            (CopyResult::IOCompleted(10), BufferStateChange::None),
            buffer.read_from(&mut cursor)
        );
    }

    #[test]
    fn input_with_full_read_page() {
        let input1: Vec<u8> = (0..BUFFER_CAPACITY).map(|x| x as u8).collect();
        let input2: Vec<u8> = (0..10).collect();
        let mut cursor = Cursor::new(input1);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut cursor)
        );

        cursor = Cursor::new(input2);
        assert_eq!(
            (CopyResult::IOCompleted(10), BufferStateChange::None),
            buffer.read_from(&mut cursor)
        );
    }

    #[test]
    fn input_split_across_read_and_write_page() {
        let buffer_and_half = BUFFER_CAPACITY + (BUFFER_CAPACITY / 2);
        let input1: Vec<u8> = (0..buffer_and_half).map(|x| x as u8).collect();
        let mut cursor = Cursor::new(input1);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut cursor)
        );
        assert_eq!(
            (
                CopyResult::IOCompleted(buffer_and_half - BUFFER_CAPACITY),
                BufferStateChange::None
            ),
            buffer.read_from(&mut cursor)
        );
    }

    #[test]
    fn input_full_read_and_write_pages() {
        let input1: Vec<u8> = (0..BUFFER_CAPACITY).map(|x| x as u8).collect();
        let input2: Vec<u8> = input1.iter().map(|x| *x).collect();
        let input3: Vec<u8> = (0..10).collect();
        let mut cursor = Cursor::new(input1);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut cursor)
        );

        let mut cursor = Cursor::new(input2);
        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::None
            ),
            buffer.read_from(&mut cursor)
        );

        let mut cursor = Cursor::new(input3);
        assert_eq!(
            (CopyResult::BufferNotReady, BufferStateChange::None),
            buffer.read_from(&mut cursor)
        );
    }

    #[test]
    fn output_empty_buffer() {
        let mut output = Vec::new();
        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (CopyResult::BufferNotReady, BufferStateChange::None),
            buffer.write_to(&mut output)
        );
    }

    #[test]
    fn output_partial_write_page() {
        let input: Vec<u8> = (0..10).collect();
        let mut output = Cursor::new([0u8; 10]);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(10),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut Cursor::new(input))
        );

        assert_eq!(
            (CopyResult::IOCompleted(10), BufferStateChange::BecameEmpty),
            buffer.write_to(&mut output)
        );

        assert_eq!(
            (0..10).collect::<Vec<u8>>(),
            output.get_ref().iter().map(|x| *x).collect::<Vec<u8>>()
        );
    }

    #[test]
    fn output_partial_write_page_small_buffer() {
        let input1: Vec<u8> = (0..7).collect(); // Don't use the exact same sizes on read and write.
        let input2: Vec<u8> = (7..10).collect(); // This shouldn't matter but be paranoid.
        let mut output = Cursor::new([0u8; 5]);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(7),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut Cursor::new(input1))
        );

        assert_eq!(
            (CopyResult::IOCompleted(3), BufferStateChange::None),
            buffer.read_from(&mut Cursor::new(input2))
        );

        assert_eq!(
            (CopyResult::IOCompleted(5), BufferStateChange::None),
            buffer.write_to(&mut output)
        );

        assert_eq!(
            (0..5).collect::<Vec<u8>>(),
            output.get_ref().iter().map(|x| *x).collect::<Vec<u8>>()
        );

        output.set_position(0);
        assert_eq!(
            (CopyResult::IOCompleted(5), BufferStateChange::BecameEmpty),
            buffer.write_to(&mut output)
        );

        assert_eq!(
            (5..10).collect::<Vec<u8>>(),
            output.get_ref().iter().map(|x| *x).collect::<Vec<u8>>()
        );
    }

    #[test]
    fn output_split_across_write_and_read_page() {
        let buffer_and_half = BUFFER_CAPACITY + (BUFFER_CAPACITY / 2);
        let input: Vec<u8> = (0..buffer_and_half).map(|x| x as u8).collect();
        let mut cursor1 = Cursor::new(input);

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut cursor1)
        );
        assert_eq!(
            (
                CopyResult::IOCompleted(buffer_and_half - BUFFER_CAPACITY),
                BufferStateChange::None
            ),
            buffer.read_from(&mut cursor1)
        );

        let mut cursor2 = Cursor::new(heap_buffer(buffer_and_half));
        let mut cursor2_check = heap_buffer(buffer_and_half);
        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::None
            ),
            buffer.write_to(&mut cursor2)
        );

        // The output buffer has a filled and unfilled portion, only init the
        // filled portion since the unfilled will be 0
        for i in 0..BUFFER_CAPACITY {
            cursor2_check[i] = i as u8;
        }

        assert_eq!(
            cursor2_check.iter().map(|x| *x).collect::<Vec<u8>>(),
            cursor2.get_ref().iter().map(|x| *x).collect::<Vec<u8>>()
        );

        cursor2.set_position(0);
        assert_eq!(
            (
                CopyResult::IOCompleted(buffer_and_half - BUFFER_CAPACITY),
                BufferStateChange::BecameEmpty
            ),
            buffer.write_to(&mut cursor2)
        );

        // The output buffer has a filled and unfilled portion, only init the
        // filled portion since the unfilled is retained from the last write
        let filled = buffer_and_half - BUFFER_CAPACITY;
        for i in 0..filled {
            cursor2_check[i] = (i + BUFFER_CAPACITY) as u8;
        }

        assert_eq!(
            cursor2_check.iter().map(|x| *x).collect::<Vec<u8>>(),
            cursor2.get_ref().iter().map(|x| *x).collect::<Vec<u8>>()
        );
    }

    #[test]
    fn discard_empty() {
        let mut buffer = FlipBuffer::new();
        assert_eq!(BufferStateChange::None, buffer.discard());

        let mut output = Vec::new();
        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (CopyResult::BufferNotReady, BufferStateChange::None),
            buffer.write_to(&mut output)
        );
    }

    #[test]
    fn discard_write_buffer_no_flip() {
        let input: Vec<u8> = (0..10).collect();
        let mut output = Vec::new();

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(10),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut Cursor::new(input))
        );

        assert_eq!(BufferStateChange::BecameEmpty, buffer.discard());

        assert_eq!(
            (CopyResult::BufferNotReady, BufferStateChange::None),
            buffer.write_to(&mut output)
        );
    }

    #[test]
    fn discard_write_buffer_with_flip() {
        let input1: Vec<u8> = (0..BUFFER_CAPACITY).map(|x| x as u8).collect();
        let input2: Vec<u8> = (0..10).collect();
        let mut output = Vec::new();

        let mut buffer = FlipBuffer::new();
        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::BecameNonEmpty
            ),
            buffer.read_from(&mut Cursor::new(input1))
        );

        assert_eq!(
            (CopyResult::IOCompleted(10), BufferStateChange::None),
            buffer.read_from(&mut Cursor::new(input2))
        );

        assert_eq!(BufferStateChange::None, buffer.discard());

        assert_eq!(
            (
                CopyResult::IOCompleted(BUFFER_CAPACITY),
                BufferStateChange::BecameEmpty
            ),
            buffer.write_to(&mut output)
        );

        assert_eq!(
            (0..BUFFER_CAPACITY).map(|x| x as u8).collect::<Vec<u8>>(),
            output
        );
    }
}
