//! Write-Ahead Log
use tokio::codec::{FramedRead,FramedWrite,Decoder,Encoder};
use crc::crc32;
use bytes::{BytesMut, BufMut};
use byteorder::{ByteOrder,BigEndian};
use std::collections::{HashSet, HashMap};
use atomic_refcell::AtomicRefCell;
use std::path::Path;
use super::*;

#[derive(Debug)]
enum WalError {
    FileNotFound,
    UnknownRecordType,
    IncompleteRecord,
    LabelNotUtf8,
    TooManyLabels,
    ZeroLabels,
    DuplicateLabel,
    InvalidRecordLength,
    CrcFailure,
    Io(io::Error)
}

impl From<io::Error> for WalError {
    fn from(e: io::Error) -> WalError {
        match e.kind() {
            io::ErrorKind::NotFound => Self::FileNotFound,
            _ => Self::Io(e)
        }
    }
}

struct LabelSetEntry {
    data: BytesMut,
}
impl LabelSetEntry {
    fn new(label: &str, layer: [u32;5]) -> Self {
        let label_bytes = label.as_bytes();
        if label_bytes.len() >= 256 {
            panic!("label is too long");
        }
        let label_len = label_bytes.len() as u8;

        let mut layer_bytes = [0;20];
        BigEndian::write_u32(&mut layer_bytes[0..4], layer[0]);
        BigEndian::write_u32(&mut layer_bytes[4..8], layer[1]);
        BigEndian::write_u32(&mut layer_bytes[8..12], layer[2]);
        BigEndian::write_u32(&mut layer_bytes[12..16], layer[3]);
        BigEndian::write_u32(&mut layer_bytes[16..20], layer[4]);
        let total_len = label_bytes.len() + 21;

        let mut data = BytesMut::with_capacity(total_len);
        data.put(label_len);
        data.put(label_bytes);
        data.put(layer_bytes.as_ref());
        
        Self {
            data
        }
    }

    fn from_bytes(data: BytesMut) -> Result<Self,WalError> {
        let len = data[0] as usize;
        let total_len = len + 21;

        if data.len() != total_len {
            panic!("given a bytesmut of wrong size");
        }

        let label_slice = &data[1..len+1];
        std::str::from_utf8(label_slice)
            .map_err(|_|WalError::LabelNotUtf8)?;
        
        Ok(Self { data })
    }

    fn try_split_from_bytes(data: &mut BytesMut) -> Result<Self, WalError> {
        if data.len() == 0 {
            return Err(WalError::IncompleteRecord);
        }

        let entry_len = data[0] as usize + 21;
        if data.len() < entry_len {
            return Err(WalError::IncompleteRecord);
        }

        let entry_bytes = data.split_to(entry_len);

        LabelSetEntry::from_bytes(entry_bytes)
    }

    fn label_len(&self) -> usize {
        self.data[0] as usize
    }
    fn label(&self) -> String {
        let slice = &self.data[1..1+self.label_len()];
        String::from_utf8(slice.to_vec()).unwrap()
    }

    fn layer(&self) -> [u32;5] {
        let len = self.label_len();
        let offset = len+1;
        let n1 = BigEndian::read_u32(&self.data[offset..offset+4]);
        let n2 = BigEndian::read_u32(&self.data[offset+4..offset+8]);
        let n3 = BigEndian::read_u32(&self.data[offset+8..offset+12]);
        let n4 = BigEndian::read_u32(&self.data[offset+12..offset+16]);
        let n5 = BigEndian::read_u32(&self.data[offset+16..offset+20]);

        [n1,n2,n3,n4,n5]
    }
}

struct LabelSetRecord {
    data: BytesMut,
}

impl LabelSetRecord {
    fn new(index: u32, entries: Vec<LabelSetEntry>) -> Self {
        if entries.len() > 100 {
            panic!("only 100 labels allowed in a label set record");
        }
        let mut index_bytes = [0u8;4];
        BigEndian::write_u32(&mut index_bytes, index);

        let mut l = BytesMut::with_capacity(1);
        l.put(entries.len() as u8);
        let mut b = BytesMut::with_capacity(4);
        b.put(index_bytes.as_ref());

        Self::new_bytes(l, b, entries)
    }

    fn new_bytes(len_bytes: BytesMut, index_bytes: BytesMut, entries: Vec<LabelSetEntry>) -> Self {
        let len = len_bytes[0] as usize;
        if entries.len() != len {
            panic!("length of entries vec does not match length given in BytesMut");
        }

        let sum_entry_lengths: usize = entries.iter()
            .map(|e|e.data.len())
            .sum();

        let total_len = sum_entry_lengths + 5;
        let mut data = len_bytes;

        let mut entries_bytes = BytesMut::new();
        for entry in entries.into_iter() {
            entries_bytes.unsplit(entry.data);
        }

        data.unsplit(entries_bytes);
        data.unsplit(index_bytes);

        Self { data }
    }

    fn try_split_from_bytes(data: &mut BytesMut) -> Result<Self,WalError> {
        let mut clone = data.clone();
        if data.len() == 0 {
            return Err(WalError::IncompleteRecord);
        }

        let len = data[0] as usize;
        if len > 100 {
            return Err(WalError::TooManyLabels);
        }

        data.advance(1);

        for _i in 0..len {
            LabelSetEntry::try_split_from_bytes(data)?;
        }

        if data.len() < 4 {
            return Err(WalError::IncompleteRecord);
        }

        data.advance(4);
        
        Ok(Self { data: clone.split_to(clone.len()-data.len()) })
    }

    fn from_bytes(data: BytesMut) -> Self {
        let mut cloned = data.clone();
        let result = Self::try_split_from_bytes(&mut cloned).unwrap();

        if cloned.len() != 0 {
            panic!("bytes left after converting record from bytes");
        }

        result
    }

    fn len(&self) -> usize {
        self.data[0] as usize
    }

    fn entries(&self) -> impl Iterator<Item=LabelSetEntry> {
        let entries = self.data.clone().split_off(1);
        (0..self.len())
            .scan(entries, |data, _i| Some(LabelSetEntry::try_split_from_bytes(data).unwrap()))
    }

    fn identifier(&self) -> u32 {
        BigEndian::read_u32(&self.data[self.data.len()-4..])
    }
}

struct CheckpointRecord {
    data: BytesMut
}

impl CheckpointRecord {
    fn new(index: u32) -> Self {
        let mut bytes = [0u8;4];
        BigEndian::write_u32(&mut bytes, index);
        let mut data = BytesMut::with_capacity(4);
        data.put(bytes.as_ref());

        Self { data }
    }

    fn from_bytes(index_bytes: BytesMut) -> Self {
        if index_bytes.len() != 4 {
            panic!("CheckpointRecord made with buf of length other than 4 bytes");
        }

        Self { data: index_bytes }
    }

    fn try_split_from_bytes(data: &mut BytesMut) -> Result<Self,WalError> {
        if data.len() < 4 {
            return Err(WalError::IncompleteRecord);
        }

        Ok(Self::from_bytes(data.split_to(4)))
    }

    fn identifier(&self) -> u32 {
        BigEndian::read_u32(&self.data)
    }
}

enum WalRecord {
    LabelSet(LabelSetRecord),
    Checkpoint(CheckpointRecord)
}

impl WalRecord {
    fn into_data(self) -> BytesMut {
        match self {
            Self::LabelSet(r) => r.data,
            Self::Checkpoint(r) => r.data
        }
    }

    fn type_num(&self) -> u8 {
        match self {
            Self::LabelSet(_) => 0,
            Self::Checkpoint(_) => 1
        }
    }

    fn data(&self) -> &[u8] {
        match self {
            Self::LabelSet(r) => r.data.as_ref(),
            Self::Checkpoint(r) => r.data.as_ref()
        }
    }

    fn length(&self) -> usize {
        self.data().len()
    }
    fn checksum(&self) -> u32 {
        crc32::checksum_ieee(self.data())
    }
}

struct AnnotatedWalRecord {
    data: BytesMut
}

impl AnnotatedWalRecord {
    fn new(record: WalRecord) -> Self {
        let tp = match record {
            WalRecord::LabelSet(_) => 0u8,
            WalRecord::Checkpoint(_) => 1u8
        };

        let mut type_bytes = BytesMut::with_capacity(1);
        type_bytes.put(tp);

        let mut len_bytes = BytesMut::with_capacity(4);
        len_bytes.put([0,0,0,0].as_ref());
        BigEndian::write_u32(len_bytes.as_mut(), record.data().len() as u32);

        let mut checksum_bytes = BytesMut::with_capacity(4);
        checksum_bytes.put([0,0,0,0].as_ref());
        BigEndian::write_u32(checksum_bytes.as_mut(), record.checksum());

        Self::new_bytes(record, type_bytes, len_bytes, checksum_bytes)
    }

    fn from_bytes_unchecked(data: BytesMut) -> Self {
        AnnotatedWalRecord { data }
    }

    fn new_bytes(record: WalRecord, type_bytes: BytesMut, len_bytes: BytesMut, checksum_bytes: BytesMut) -> Self {
        if type_bytes.len() != 1 {
            panic!("type too long");
        }

        if len_bytes.len() != 4 {
            panic!("len bytes too long");
        }

        if checksum_bytes.len() != 4 {
            panic!("checksum bytes too long");
        }

        let len = BigEndian::read_u32(len_bytes.as_ref()) as usize;
        if len != record.data().len() {
            panic!("received length doesn't match data length");
        }

        let checksum = BigEndian::read_u32(checksum_bytes.as_ref());
        if checksum != record.checksum() {
            panic!("received checksum doesn't match computed checksum");
        }

        let mut data = type_bytes;
        data.unsplit(record.into_data());
        data.unsplit(len_bytes);
        data.unsplit(checksum_bytes);

        Self { data }
    }

    fn record_bytes(&self) -> BytesMut {
        let mut record_bytes = self.data.clone();
        record_bytes.advance(1);
        record_bytes.truncate(record_bytes.len()-8);

        record_bytes
    }

    fn record_len(&self) -> usize {
        let slice = &self.data[self.data.len()-8..self.data.len()-4];
        BigEndian::read_u32(slice) as usize
    }

    fn checksum(&self) -> u32 {
        let slice = &self.data[self.data.len()-4..];
        BigEndian::read_u32(slice)
    }
    
    fn record(&self) -> WalRecord {
        let tp = self.data[0];
        match tp {
            0 => WalRecord::LabelSet(LabelSetRecord::from_bytes(self.record_bytes())),
            1 => WalRecord::Checkpoint(CheckpointRecord::from_bytes(self.record_bytes())),
            _ => panic!("invalid record type {}", tp)
        }
    }
}

const WAL_FILE_NAME: &'static str = "wa.log";
struct SharedWalFile {
    file: LockedFile,
}

impl SharedWalFile {
    pub fn open<P:'static+AsRef<Path>+Send>(path: P) -> impl Future<Item=Self,Error=io::Error>+Send {
        LockedFile::create_and_open(path)
            .map(|file| Self { file } )
    }
}

trait ReadableWalFile: 'static+Sized+Send {
    type R: 'static+AsyncRead+FutureSeekable+Send;

    fn file(self) -> Self::R;
    fn from_file(file: Self::R) -> Self;

    fn seek(self, from: SeekFrom) -> Box<dyn Future<Item=(Self, u64), Error=WalError>+Send> {
        Box::new(self.file().seek(from)
                 .map(|(file, pos)| (Self::from_file(file), pos))
                 .map_err(|e|e.into()))
    }

    fn into_records_stream(self) -> Box<dyn Stream<Item=AnnotatedWalRecord,Error=WalError>+Send> {
        Box::new(FramedRead::new(self.file(), WalFileDecoder::Start))
    }

    fn next_record(self) -> Box<dyn Future<Item=(Option<AnnotatedWalRecord>,Self),Error=(WalError,Self)>+Send> {
        Box::new(FramedRead::new(self.file(), WalFileDecoder::Start)
                 .into_future()
                 .map(|(record, framed)|(record, Self::from_file(framed.into_inner())))
                 .map_err(|(e, framed)|(e, Self::from_file(framed.into_inner()))))
    }

    fn peek_record(self) -> Box<dyn Future<Item=(Option<AnnotatedWalRecord>,Self),Error=(WalError, Option<Self>)>+Send> {
        Box::new(self.seek(SeekFrom::Current(0))
                 .map_err(|e|(e, None))
                 .and_then(|(wal, start)|
                           wal.next_record()
                           .then(move |result| match result {
                               Ok((r,wal)) => future::Either::A(
                                   wal.seek(SeekFrom::Start(start))
                                       .map_err(|e|(e, None))
                                       .map(move |(wal, _pos)| (r, wal))),
                               Err((e, wal)) => future::Either::B(
                                   wal.seek(SeekFrom::Start(start))
                                       .map_err(|e|(e, None))
                                       .and_then(move |(file, _pos)|
                                                 future::err((e, Some(file))))),
                           })))
    }

    fn seek_previous(self) -> Box<dyn Future<Item=(Self, u64),Error=(WalError,Option<Self>)>+Send> {
        Box::new(self.seek(SeekFrom::Current(0))
                 .map_err(|e|(e.into(), None))
                 .and_then(|(wal, pos)| {
                     if pos == 0 {
                         future::Either::A(future::ok((wal, pos)))
                     }
                     else if pos < 8 {
                         future::Either::A(future::err((WalError::IncompleteRecord,
                                                        Some(wal))))
                     }
                     else {
                         future::Either::B(
                             wal.seek(SeekFrom::Current(-8))
                                 .map_err(|e|(e.into(), None))
                                 .and_then(|(wal, pos)|
                                           tokio::io::read_exact(wal.file(), vec![0;4])
                                           .map_err(|e|(e.into(), None))
                                           .and_then(move |(file, buf)| {
                                               let len = BigEndian::read_u32(&buf) as u64;
                                               if pos < len + 8 {
                                                   future::Either::A(
                                                       future::err((WalError::IncompleteRecord,
                                                                    Some(Self::from_file(file)))))
                                               }
                                               else {
                                                   future::Either::B(
                                                       file.seek(SeekFrom::Start(pos - len - 8))
                                                           .map_err(|e|(e.into(), None)))
                                               }
                                           })
                                           .map(|(file, pos)| (Self::from_file(file), pos))))
                     }
                 }))
    }

    fn peek_previous(self) -> Box<dyn Future<Item=(Option<AnnotatedWalRecord>,Self),Error=(WalError,Option<Self>)>+Send> {
        Box::new(self.seek(SeekFrom::Current(0))
                 .map_err(|e|(e,None))
                 .and_then(|(wal, current)| match current {
                     0 => future::Either::A(future::ok((None, wal))),
                     current => future::Either::B(wal.seek_previous()
                                                  .and_then(|(wal, _previous)| wal.next_record().map_err(|(e, wal)|(e, Some(wal))))
                                                  .and_then(move |(record, wal)| match record {
                                                      Some(record) => future::Either::A(wal.seek(SeekFrom::Current(0))
                                                                                        .map_err(|e| (e, None))
                                                                                        .and_then(move |(wal, new_current)| match current == new_current {
                                                                                            true => future::ok((Some(record), wal)),
                                                                                            false => future::err((WalError::InvalidRecordLength, Some(wal)))
                                                                                        })),
                                                      None => future::Either::B(future::ok((None, wal)))
                                                  }))
                 }))
    }

    fn previous(self) -> Box<dyn Future<Item=(Option<AnnotatedWalRecord>,Self),Error=(WalError,Option<Self>)>+Send> {
        Box::new(self.peek_previous()
                 .and_then(|(record, wal)| wal.seek_previous()
                           .map(move |(wal, _previous)| (record, wal))))
    }

    fn walk_backwards<T:'static+Send>(self, mut call: impl 'static + FnMut(Option<AnnotatedWalRecord>) -> Option<T> + Send) -> Box<dyn Future<Item=(Self,Option<T>), Error=(WalError, Option<Self>)>+Send> {
        Box::new(self.previous()
                 .and_then(|(record, wal)| match (record.is_some(), call(record)) {
                     (true, None) => future::Either::A(wal.walk_backwards(call)),
                     (_, Some(result)) => future::Either::B(future::ok((wal, Some(result)))),
                     _ => future::Either::B(future::ok((wal, None)))
                 }))
    }

    fn get_labels_since_last_checkpoint(self, mut labels: HashSet<String>) -> Box<dyn Future<Item=(HashMap<String, (u32, [u32;5])>, u32, Self), Error=(WalError,Option<Self>)>+Send> {
        let checkpoint: AtomicRefCell<Option<u32>> = AtomicRefCell::new(None);
        let discovered = AtomicRefCell::new(HashMap::new());
        let cloned = discovered.clone();
        let f = move |record:Option<AnnotatedWalRecord>| match record.map(|r|r.record()) {
            Some(WalRecord::Checkpoint(record)) => {
                let mut checkpoint = checkpoint.borrow_mut();
                match *checkpoint {
                    None => *checkpoint = Some(record.identifier()),
                    Some(_) => {}
                }

                None
            },
            Some(WalRecord::LabelSet(record)) => {
                let id = record.identifier();
                if Some(id) == *checkpoint.borrow() {
                    // this is the checkpointed record. anything before this has been written to the label files.
                    Some(id)
                }
                else {
                    for entry in record.entries() {
                        let mut discovered = discovered.borrow_mut();
                        let label = entry.label();
                        if labels.remove(&label) {
                            discovered.insert(label, (id, entry.layer()));
                        }
                    }

                    None
                }
            },
            None => Some(match *checkpoint.borrow() {
                Some(id) => id,
                None => 0
            })
        };

        Box::new(self.walk_backwards(f)
                 .map(|(wal, checkpoint_id)| (cloned.into_inner(), checkpoint_id.unwrap_or(0), wal)))
    }

    fn get_all_labels_since_last_checkpoint(self) -> Box<dyn Future<Item=(HashMap<String, (u32, [u32;5])>, u32, Self), Error=(WalError,Option<Self>)>+Send> {
        let checkpoint: AtomicRefCell<Option<u32>> = AtomicRefCell::new(None);
        let discovered = AtomicRefCell::new(HashMap::new());
        let cloned = discovered.clone();
        let f = move |record:Option<AnnotatedWalRecord>| match record.map(|r|r.record()) {
            Some(WalRecord::Checkpoint(record)) => {
                let mut checkpoint = checkpoint.borrow_mut();
                match *checkpoint {
                    None => *checkpoint = Some(record.identifier()),
                    Some(_) => {}
                }

                None
            },
            Some(WalRecord::LabelSet(record)) => {
                let id = record.identifier();
                if Some(id) == *checkpoint.borrow() {
                    // this is the checkpointed record. anything before this has been written to the label files.
                    Some(id)
                }
                else {
                    for entry in record.entries() {
                        let mut discovered = discovered.borrow_mut();
                        let label = entry.label();
                        if !discovered.contains_key(&label) {
                            discovered.insert(label, (id, entry.layer()));
                        }
                    }

                    None
                }
            },
            None => Some(match *checkpoint.borrow() {
                Some(id) => id,
                None => 0
            })
        };

        Box::new(self.walk_backwards(f)
                 .map(|(wal, checkpoint_id)| (cloned.into_inner(), checkpoint_id.unwrap_or(0), wal)))
    }

    fn get_last_checkpoint(self) -> Box<dyn Future<Item=(Self, u32), Error=(WalError,Option<Self>)>+Send> {
        Box::new(self.walk_backwards(|record| match record.map(|r|r.record()) {
            Some(WalRecord::Checkpoint(record)) => Some(record.identifier()),
            Some(_) => None,
            // no checkpoint, so we use 0, which is always going to be less than an actual id
            None => Some(0)
        })
                 .map(|(wal, id)|(wal, id.unwrap())))
    }

    fn get_last_id(self) -> Box<dyn Future<Item=(Self, u32), Error=(WalError,Option<Self>)>+Send> {
        Box::new(self.walk_backwards(|record| match record.map(|r|r.record()) {
            Some(WalRecord::LabelSet(record)) => Some(record.identifier()),
            Some(_) => None,
            None => None
        })
                 .and_then(|(wal, id)| match id {
                     // this can occur if we got a cleaned wal-file with only a checkpoint remaining
                     None => future::Either::A(wal.get_last_checkpoint()),
                     Some(id) => future::Either::B(future::ok((wal, id)))
                 }))
    }
}

impl ReadableWalFile for SharedWalFile {
    type R = LockedFile;

    fn file(self) -> LockedFile {
        self.file
    }

    fn from_file(file: LockedFile) -> SharedWalFile {
        SharedWalFile { file }
    }
}

impl ReadableWalFile for ExclusiveWalFile {
    type R = ExclusiveLockedFile;

    fn file(self) -> ExclusiveLockedFile {
        self.file
    }

    fn from_file(file: ExclusiveLockedFile) -> ExclusiveWalFile {
        ExclusiveWalFile { file }
    }
}

enum WalFileDecoder {
    Invalid,
    Start,
    LabelSetReadNumEntries(BytesMut),
    LabelSetReadEntry(BytesMut, u8),
    LabelSetReadIdentifier(BytesMut),

    CheckpointReadIdentifier(BytesMut),

    ReadRecordLength(BytesMut),
    ReadRecordChecksum(BytesMut, BytesMut)
}

impl Decoder for WalFileDecoder {
    type Item = AnnotatedWalRecord;
    type Error = WalError;

    fn decode(&mut self, bytes: &mut BytesMut) -> Result<Option<AnnotatedWalRecord>, WalError> {
        let mut state = Self::Invalid;
        let mut result = None;
        std::mem::swap(&mut state, self);
        loop {
            match state {
                Self::Invalid => panic!("encountered self in invalid state"),
                Self::Start => {
                    // read a byte to find out the type of the next record
                    if bytes.len() == 0 {
                        break;
                    }
                    let tp = bytes[0];
                    match tp {
                        0 => state = Self::LabelSetReadNumEntries(bytes.split_to(1)),
                        1 => state = Self::CheckpointReadIdentifier(bytes.split_to(1)),
                        _ => return Err(WalError::UnknownRecordType)
                    };
                },
                Self::LabelSetReadNumEntries(mut tp) => {
                    if bytes.len() == 0 {
                        state = Self::LabelSetReadNumEntries(tp);
                        break;
                    }
                    let len = bytes[0];
                    if len == 0 {
                        return Err(WalError::ZeroLabels);
                    }
                    if len > 100 {
                        return Err(WalError::TooManyLabels);
                    }

                    tp.unsplit(bytes.split_to(1));

                    state = Self::LabelSetReadEntry(tp, len);
                },
                Self::LabelSetReadEntry(mut read_so_far, mut num_entries) => match LabelSetEntry::try_split_from_bytes(bytes) {
                    Ok(entry) => {
                        read_so_far.unsplit(entry.data);
                        num_entries -= 1;
                        if num_entries == 0 {
                            // this was the last entry, so all entries were read
                            state = Self::LabelSetReadIdentifier(read_so_far);
                        }
                        else {
                            state = Self::LabelSetReadEntry(read_so_far, num_entries);
                        }
                    },
                    Err(WalError::IncompleteRecord) => {
                        state = Self::LabelSetReadEntry(read_so_far, num_entries);
                        break;
                    }
                    Err(e) => return Err(e)
                },
                Self::LabelSetReadIdentifier(mut read_so_far) => {
                    if bytes.len() < 4 {
                        state = Self::LabelSetReadIdentifier(read_so_far);
                        break;
                    }
                    else {
                        let identifier_bytes = bytes.split_to(4);
                        read_so_far.unsplit(identifier_bytes);

                        state = Self::ReadRecordLength(read_so_far);
                    }
                },
                Self::CheckpointReadIdentifier(mut read_so_far) => {
                    if bytes.len() < 4 {
                        state = Self::CheckpointReadIdentifier(read_so_far);
                        break;
                    }
                    else {
                        let identifier_bytes = bytes.split_to(4);
                        read_so_far.unsplit(identifier_bytes);

                        state = Self::ReadRecordLength(read_so_far);
                    }
                },
                Self::ReadRecordLength(record_bytes) => {
                    if bytes.len() < 4 {
                        state = Self::ReadRecordLength(record_bytes);
                        break;
                    }
                    else {
                        let record_length_bytes = bytes.split_to(4);

                        state = Self::ReadRecordChecksum(record_bytes, record_length_bytes);
                    }
                },
                Self::ReadRecordChecksum(record_bytes, record_length_bytes) => {
                    if bytes.len() < 4 {
                        state = Self::ReadRecordChecksum(record_bytes, record_length_bytes);
                        break;
                    }
                    else {
                        let record_checksum_bytes = bytes.split_to(4);

                        let record_length = BigEndian::read_u32(record_length_bytes.as_ref()) as usize;
                        if record_length != record_bytes.len() {
                            return Err(WalError::InvalidRecordLength);
                        }
                        let record_checksum = BigEndian::read_u32(record_checksum_bytes.as_ref());
                        let computed_checksum = crc32::checksum_ieee(record_bytes.as_ref());
                        if record_checksum != computed_checksum {
                            return Err(WalError::CrcFailure);
                        }

                        let mut data = record_bytes;
                        data.advance(1);

                        state = Self::Start;
                        result = Some(AnnotatedWalRecord::from_bytes_unchecked(data));
                        break;
                    }
                }
                    
            }
        }

        std::mem::swap(&mut state, self);

        Ok(result)
    }
}

struct WalFileEncoder;

impl Encoder for WalFileEncoder {
    type Item = AnnotatedWalRecord;
    type Error = io::Error;

    fn encode(&mut self, record: AnnotatedWalRecord, bytes: &mut BytesMut) -> Result<(), io::Error> {
        bytes.unsplit(record.data);

        Ok(())
    }
}

struct ExclusiveWalFile {
    file: ExclusiveLockedFile,
}

impl ExclusiveWalFile {
    fn append_labelset(self, id: u32, labels: HashMap<String, [u32;5]>) -> impl Future<Item=bool,Error=WalError>+Send {
        self.get_last_id()
            .map_err(|(e, _)|e)
            .and_then(move |(wal, last_id)| match id == last_id+1 {
                false => future::Either::A(future::ok(false)),
                true => future::Either::B(FramedWrite::new(wal.file(), WalFileEncoder)
                                          .send(AnnotatedWalRecord::new(WalRecord::LabelSet(
                                              LabelSetRecord::new(id,labels.into_iter()
                                                                  .map(|(label,layer)| LabelSetEntry::new(&label, layer))
                                                                  .collect()))))
                                          .and_then(|sink|sink.into_inner().do_shutdown())
                                          .map_err(|e|e.into())
                                          .map(|_|true))
            })
    }

    fn append_checkpoint(self, checkpoint: u32) -> impl Future<Item=bool,Error=WalError>+Send {
        self.get_last_checkpoint()
            .map_err(|(e, _)|e)
            .and_then(move |(wal, last_checkpoint)| match checkpoint > last_checkpoint {
                false => future::Either::A(future::ok(false)),
                true => future::Either::B(FramedWrite::new(wal.file(), WalFileEncoder)
                                          .send(AnnotatedWalRecord::new(WalRecord::Checkpoint(CheckpointRecord::new(checkpoint))))
                                          .and_then(|sink|sink.into_inner().do_shutdown())
                                          .map_err(|e|e.into())
                                          .map(|_|true))
            })
    }

    fn truncate(self) -> impl Future<Item=(),Error=WalError>+Send {
        // ensure we can peek previous record (so we're probably at a valid point in the stream)
        self.peek_previous()
            .map_err(|(e,_)|e)
            .and_then(|(_, wal)| wal.file.truncate()
                      .and_then(|f| f.do_shutdown())
                      .map_err(|e|e.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;
    #[test]
    fn empty_file_next_record_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let wal_path = dir.path().join("wa.log");

        let wal = SharedWalFile::open(wal_path);
    }
}