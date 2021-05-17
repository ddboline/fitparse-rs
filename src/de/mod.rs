//! Deserialize a stream of FIT file data into the serde data model by parsing the file and
//! applying the packaged FIT profile to the data.
use crate::error::{ErrorKind, Result};
use crate::FitDataRecord;
use nom::number::complete::le_u16;
use std::collections::HashMap;
use std::io::Read;
use std::rc::Rc;
use log::warn;

mod crc;
use crc::{caculate_crc, update_crc};
mod decode;
use decode::Decoder;
mod parser;
pub use parser::{FitDataMessage, FitDefinitionMessage, FitFileHeader};

/// Stores a FIT file object (header, message or CRC)
#[derive(Clone, Debug)]
pub enum FitObject {
    /// Checksum at end of data section
    Crc(u16),
    /// Header containing FIT file info
    Header(FitFileHeader),
    /// A raw data message
    DataMessage(FitDataMessage),
    /// A definition message used to define upcoming data messages
    DefinitionMessage(Rc<FitDefinitionMessage>),
}

/// Manages the deserialization of a FIT data stream into Rust constructs.
struct Deserializer {
    /// Track the current set of FIT message definitions, these are what allows the format to
    /// be self describing.
    definitions: HashMap<u8, Rc<parser::FitDefinitionMessage>>,
    /// Stores the current position in the byte stream, this is needed for error generation and
    /// checking the state of the parser
    position: usize,
    /// Stores the location that the current FIT message ends, for chained FIT messges this will
    /// be updated to reflect the new end position
    end_of_messages: usize,
    /// Stores the current CRC value
    crc: u16,
}

impl Deserializer {
    /// Create the deserializer with an empty state
    fn new() -> Self {
        Deserializer {
            definitions: HashMap::new(),
            position: 0,
            end_of_messages: 0,
            crc: 0,
        }
    }

    /// Clear the definition messages used to decode data messages and reset the CRC value. This
    /// can be called between distinct FIT files but if they are properly formed it should not be
    /// necessary since new definitions will replace the old in the mapping.
    fn reset(&mut self) {
        self.crc = 0;
        self.definitions = HashMap::new();
    }

    /// Advance the parser state returning one of four possible objects defined within the
    /// FIT file.
    fn deserialize_next<'de>(&mut self, input: &'de [u8]) -> Result<(&'de [u8], FitObject)> {
        if self.position > 0 && self.position == self.end_of_messages {
            // extract the CRC
            return self.deserialize_crc(input);
        }
        if self.position == 0 || (self.position > self.end_of_messages && !input.is_empty()) {
            // if there is extra bytes remaining the FIT file must be chained so we parse
            // the header and continue on.
            return self.deserialize_header(input);
        }
        // if we reach this point then we must be at some position: 0 < X < self.end_of_messages
        // and a message should exist (either data or definition).
        self.deserialize_message(input)
    }

    /// Parse the FIT header
    fn deserialize_header<'de>(&mut self, input: &'de [u8]) -> Result<(&'de [u8], FitObject)> {
        let (remaining, header) =
            parser::fit_file_header(input).map_err(|e| self.to_parse_err(e))?;
        self.end_of_messages =
            self.position + header.header_size() as usize + header.data_size() as usize;
        self.position += header.header_size() as usize;
        self.crc = 0;

        if let Some(value) = header.crc() {
            let checksum = caculate_crc(&input[0..(header.header_size() - 2) as usize]);
            if checksum != value {
                warn!("invalid checksum header {} {}", checksum, value);
                // return Err(Box::new(ErrorKind::InvalidCrc((
                //     Vec::from(remaining),
                //     FitObject::Header(header),
                //     value,
                //     checksum,
                // ))));
            }
        } else {
            // if the header doesn't have its own CRC then the header bytes are included in
            // the data CRC
            self.crc = update_crc(0, &input[0..(header.header_size() as usize)]);
        }

        Ok((remaining, FitObject::Header(header)))
    }

    /// Extract a 2 byte CRC
    fn deserialize_crc<'de>(&mut self, input: &'de [u8]) -> Result<(&'de [u8], FitObject)> {
        let (input, crc) = le_u16(input).map_err(|e| self.to_parse_err(e))?;
        self.position += 2;
        if crc != self.crc {
            warn!("invalid checksum crc {} {}", crc, self.crc);
            // return Err(Box::new(ErrorKind::InvalidCrc((
            //     Vec::from(input),
            //     FitObject::Crc(crc),
            //     crc,
            //     self.crc,
            // ))));
        }
        Ok((input, FitObject::Crc(crc)))
    }

    /// Parse a FIT data or definition message
    fn deserialize_message<'de>(&mut self, input: &'de [u8]) -> Result<(&'de [u8], FitObject)> {
        // parse a single message of either variety
        let init_len = input.len();
        let (remaining, message) =
            parser::fit_message(input, &self.definitions).map_err(|e| self.to_parse_err(e))?;
        // update CRC with the consumed bytes
        self.crc = update_crc(self.crc, &input[0..(input.len() - remaining.len())]);

        match message {
            parser::FitMessage::Data(message) => {
                self.position += init_len - remaining.len();
                Ok((remaining, FitObject::DataMessage(message)))
            }
            parser::FitMessage::Definition(message) => {
                // Use an Rc to avoid an expensive clone of the DefinitionMessage itself
                let msg_rc = Rc::new(message);
                self.definitions
                    .insert(msg_rc.local_message_number(), Rc::clone(&msg_rc));
                self.position += init_len - remaining.len();
                Ok((remaining, FitObject::DefinitionMessage(msg_rc)))
            }
            parser::FitMessage::MissingDefinitionMessage(n) => {
                Err(ErrorKind::MissingDefinitionMessage(n, self.position).into())
            }
        }
    }

    /// Inject the byte stream position into the Error when converting a nom parsing error. This
    /// is not easy to get using the vanilla From trait since we need outside information.
    fn to_parse_err(&self, err: nom::Err<nom::error::Error<&[u8]>>) -> crate::Error {
        match err {
            nom::Err::Error(inner_err) => ErrorKind::ParseError(self.position, inner_err.code).into(),
            nom::Err::Failure(inner_err) => ErrorKind::ParseError(self.position, inner_err.code).into(),
            nom::Err::Incomplete(needed) => ErrorKind::UnexpectedEof(needed).into(),
        }
    }
}

/// Deserialize and decode a stream of bytes
pub struct FitStreamProcessor {
    decoder: Decoder,
    deserializer: Deserializer,
}

impl Default for FitStreamProcessor {
    fn default() -> Self {
        FitStreamProcessor {
            decoder: Decoder::new(),
            deserializer: Deserializer::new(),
        }
    }
}

impl FitStreamProcessor {
    /// Create the processor
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset the decoder state and definition messages in use, this should be called at the end of
    /// each FIT file to ensure the accumlator fields in the decoder will produce the right values
    /// per file.
    pub fn reset(&mut self) {
        self.decoder.reset();
        self.deserializer.reset();
    }

    /// Deserialize a FitObject from the byte stream.
    pub fn deserialize_next<'de>(&mut self, input: &'de [u8]) -> Result<(&'de [u8], FitObject)> {
        self.deserializer.deserialize_next(input)
    }

    /// Decode a FIT data message into a FIT data record using the defined FIT profile.
    pub fn decode_message(&mut self, msg: FitDataMessage) -> Result<FitDataRecord> {
        self.decoder.decode_message(msg)
    }
}

/// Deserialize a FIT file stored as an array of bytes and return the decoded data messages.
pub fn from_bytes(mut buffer: &[u8]) -> Result<Vec<FitDataRecord>> {
    let mut processor = FitStreamProcessor::new();
    let mut records = Vec::new();

    while !buffer.is_empty() {
        let (buf, obj) = processor.deserialize_next(buffer)?;
        match obj {
            FitObject::Crc(..) => processor.reset(),
            FitObject::Header(..) => {}
            FitObject::DataMessage(msg) => records.push(processor.decode_message(msg)?),
            FitObject::DefinitionMessage(..) => {}
        }
        buffer = buf;
    }

    Ok(records)
}

/// Deserialize a FIT file stored in a source that implements io::Read.
pub fn from_reader<T: Read>(source: &mut T) -> Result<Vec<FitDataRecord>> {
    let mut buffer = Vec::new();
    source.read_to_end(&mut buffer)?;
    from_bytes(&buffer)
}
