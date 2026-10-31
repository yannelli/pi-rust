//! Natives for `node:zlib`: deflate and inflate (flate2 on zlib-rs), brotli, zstd, and crc32.

use brotli::enc::StandardAlloc;
use brotli::enc::encode::{BrotliEncoderOperation, BrotliEncoderParameter, BrotliEncoderStateStruct};
use brotli::enc::interface;
use brotli::{BrotliDecompressStream, BrotliResult, BrotliState};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use rquickjs::{Ctx, Object, TypedArray};
use zstd::stream::raw::{CParameter, DParameter, InBuffer, Operation, OutBuffer};

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_of, bytes_to_js, throw};
use crate::runtime::state::state;

// Node's mode numbers (zlib.constants.DEFLATE and so on).
const DEFLATE: u8 = 1;
pub(crate) const INFLATE: u8 = 2;
const GZIP: u8 = 3;
pub(crate) const GUNZIP: u8 = 4;
const DEFLATERAW: u8 = 5;
pub(crate) const INFLATERAW: u8 = 6;
const UNZIP: u8 = 7;
pub(crate) const BROTLI_DECODE: u8 = 8;
const BROTLI_ENCODE: u8 = 9;
const ZSTD_COMPRESS: u8 = 10;
const ZSTD_DECOMPRESS: u8 = 11;
const Z_FINISH: i32 = 4;

/// Construction settings, read from the JavaScript options object.
#[derive(Default)]
struct Settings {
    level: i32,
    window_bits: u8,
    dictionary: Option<Vec<u8>>,
    params: Vec<(u32, i64)>,
    pledged: Option<u64>,
}

type BrotliDecoder = BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>;

pub(crate) enum Codec {
    Deflate(Box<Compress>),
    Inflate {
        inner: Option<Box<Decompress>>,
        mode: u8,
        window_bits: u8,
        dictionary: Option<Vec<u8>>,
        ended: bool,
        header: Vec<u8>,
        /// Input came after the end of the stream (`rejectGarbageAfterEnd` streams fail on it).
        junk: bool,
    },
    BrotliEncode(Box<BrotliEncoderStateStruct<StandardAlloc>>),
    BrotliDecode {
        state: Box<BrotliDecoder>,
        ended: bool,
    },
    ZstdEncode(Box<zstd::stream::raw::Encoder<'static>>),
    ZstdDecode {
        inner: Box<zstd::stream::raw::Decoder<'static>>,
        boundary: bool,
        seen: bool,
    },
}

fn zerr(message: &str, code: &str, errno: i32) -> JsError {
    JsError::new(message).with_code(code).with_data("errno", errno)
}

fn eof() -> JsError {
    zerr("unexpected end of file", "Z_BUF_ERROR", -5)
}

fn level(level: i32) -> Compression {
    if level < 0 { Compression::default() } else { Compression::new(level as u32) }
}

fn new_compress(mode: u8, settings: &Settings) -> Result<Compress, JsError> {
    let bits = settings.window_bits.clamp(9, 15);
    let mut compress = match mode {
        GZIP => Compress::new_gzip(level(settings.level), bits),
        _ => Compress::new_with_window_bits(level(settings.level), mode == DEFLATE, bits),
    };
    if let Some(dictionary) = &settings.dictionary {
        compress.set_dictionary(dictionary).map_err(|_| zerr("Bad dictionary", "Z_DATA_ERROR", -3))?;
    }
    Ok(compress)
}

fn new_decompress(mode: u8, window_bits: u8, dictionary: Option<&[u8]>) -> Result<Decompress, JsError> {
    let bits = if window_bits == 0 { 15 } else { window_bits.clamp(9, 15) };
    let mut inner = match mode {
        GUNZIP => Decompress::new_gzip(bits),
        INFLATERAW => Decompress::new_with_window_bits(false, bits),
        _ => Decompress::new_with_window_bits(true, bits),
    };
    if let (INFLATERAW, Some(dictionary)) = (mode, dictionary) {
        inner.set_dictionary(dictionary).map_err(|_| zerr("Bad dictionary", "Z_DATA_ERROR", -3))?;
    }
    Ok(inner)
}

fn create(mode: u8, settings: Settings) -> Result<Codec, JsError> {
    Ok(match mode {
        DEFLATE | GZIP | DEFLATERAW => Codec::Deflate(Box::new(new_compress(mode, &settings)?)),
        INFLATE | GUNZIP | INFLATERAW | UNZIP => {
            let inner = match mode {
                UNZIP => None,
                _ => Some(Box::new(new_decompress(mode, settings.window_bits, settings.dictionary.as_deref())?)),
            };
            let window_bits = settings.window_bits;
            Codec::Inflate {
                inner,
                mode,
                window_bits,
                dictionary: settings.dictionary,
                ended: false,
                header: Vec::new(),
                junk: false,
            }
        }
        BROTLI_ENCODE => {
            let mut encoder = Box::new(BrotliEncoderStateStruct::new(StandardAlloc::default()));
            for (key, value) in settings.params {
                if let Some(param) = brotli_param(key) {
                    encoder.set_parameter(param, value as u32);
                }
            }
            Codec::BrotliEncode(encoder)
        }
        BROTLI_DECODE => {
            let alloc = StandardAlloc::default;
            Codec::BrotliDecode { state: Box::new(BrotliState::new(alloc(), alloc(), alloc())), ended: false }
        }
        ZSTD_COMPRESS => {
            let mut encoder = zstd::stream::raw::Encoder::new(3).map_err(zstd_error)?;
            for (key, value) in settings.params {
                if let Some(param) = zstd_cparam(key, value) {
                    encoder.set_parameter(param).map_err(zstd_error)?;
                }
            }
            if settings.pledged.is_some() {
                encoder.set_pledged_src_size(settings.pledged).map_err(zstd_error)?;
            }
            Codec::ZstdEncode(Box::new(encoder))
        }
        ZSTD_DECOMPRESS => {
            let mut decoder = zstd::stream::raw::Decoder::new().map_err(zstd_error)?;
            for (key, value) in settings.params {
                if key == 100 {
                    decoder.set_parameter(DParameter::WindowLogMax(value as u32)).map_err(zstd_error)?;
                }
            }
            Codec::ZstdDecode { inner: Box::new(decoder), boundary: true, seen: false }
        }
        _ => return Err(JsError::new(format!("Bad zlib mode {mode}")).with_code("ERR_INVALID_ARG_VALUE")),
    })
}

fn brotli_param(key: u32) -> Option<BrotliEncoderParameter> {
    use BrotliEncoderParameter::*;
    Some(match key {
        0 => BROTLI_PARAM_MODE,
        1 => BROTLI_PARAM_QUALITY,
        2 => BROTLI_PARAM_LGWIN,
        3 => BROTLI_PARAM_LGBLOCK,
        4 => BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING,
        5 => BROTLI_PARAM_SIZE_HINT,
        6 => BROTLI_PARAM_LARGE_WINDOW,
        _ => return None,
    })
}

fn zstd_cparam(key: u32, value: i64) -> Option<CParameter> {
    let v = value as u32;
    Some(match key {
        100 => CParameter::CompressionLevel(value as i32),
        101 => CParameter::WindowLog(v),
        102 => CParameter::HashLog(v),
        103 => CParameter::ChainLog(v),
        104 => CParameter::SearchLog(v),
        105 => CParameter::MinMatch(v),
        106 => CParameter::TargetLength(v),
        160 => CParameter::EnableLongDistanceMatching(value != 0),
        200 => CParameter::ContentSizeFlag(value != 0),
        201 => CParameter::ChecksumFlag(value != 0),
        202 => CParameter::DictIdFlag(value != 0),
        _ => return None,
    })
}

fn zstd_error(error: std::io::Error) -> JsError {
    let message = error.to_string();
    let (name, errno) = match message.as_str() {
        "Unknown frame descriptor" => ("prefix_unknown", 10),
        "Data corruption detected" => ("corruption_detected", 20),
        "Restored data doesn't match checksum" => ("checksum_wrong", 22),
        "Src size is incorrect" => ("srcSize_wrong", 72),
        "Unsupported parameter" => ("parameter_unsupported", 40),
        "Parameter is out of bound" => ("parameter_outOfBound", 42),
        _ => ("GENERIC", 1),
    };
    zerr(&message, &format!("ZSTD_error_{name}"), errno)
}

fn deflate(compress: &mut Compress, mut input: &[u8], flush: i32) -> Result<Vec<u8>, JsError> {
    let flush = match flush {
        1 => FlushCompress::Partial,
        2 => FlushCompress::Sync,
        3 => FlushCompress::Full,
        4 => FlushCompress::Finish,
        _ => FlushCompress::None,
    };
    let mut out = Vec::with_capacity(input.len() / 2 + 64);
    loop {
        out.reserve(16 * 1024);
        let before = compress.total_in();
        let status = compress
            .compress_vec(input, &mut out, flush)
            .map_err(|error| zerr(error.message().unwrap_or("Compression failed"), "Z_STREAM_ERROR", -2))?;
        input = &input[(compress.total_in() - before) as usize..];
        let full = out.len() == out.capacity();
        match status {
            Status::StreamEnd | Status::BufError if !full => break,
            Status::Ok if input.is_empty() && !full && flush != FlushCompress::Finish => break,
            _ => {}
        }
    }
    Ok(out)
}

fn inflate_step(
    inner: &mut Decompress,
    input: &mut &[u8],
    out: &mut Vec<u8>,
    dictionary: Option<&[u8]>,
) -> Result<Status, JsError> {
    out.reserve(32 * 1024);
    let before = inner.total_in();
    let result = inner.decompress_vec(input, out, FlushDecompress::None);
    *input = &input[(inner.total_in() - before) as usize..];
    match result {
        Ok(status) => Ok(status),
        Err(error) if error.needs_dictionary().is_some() => {
            let Some(dictionary) = dictionary else { return Err(zerr("Missing dictionary", "Z_NEED_DICT", 2)) };
            inner.set_dictionary(dictionary).map_err(|_| zerr("Bad dictionary", "Z_DATA_ERROR", -3))?;
            Ok(Status::Ok)
        }
        Err(error) => Err(zerr(error.message().unwrap_or("invalid data"), "Z_DATA_ERROR", -3)),
    }
}

/// A decoder with default settings, for `fetch` response bodies.
pub(crate) fn decoder(mode: u8) -> Result<Codec, JsError> {
    create(mode, Settings { window_bits: 15, ..Settings::default() })
}

impl Codec {
    pub(crate) fn process(&mut self, input: &[u8], flush: i32) -> Result<Vec<u8>, JsError> {
        match self {
            Codec::Deflate(compress) => deflate(compress, input, flush),
            Codec::Inflate { inner, mode, window_bits, dictionary, ended, header, junk } => {
                let mut out = Vec::new();
                let buffered;
                let mut input = input;
                if inner.is_none() {
                    header.extend_from_slice(input);
                    if header.len() < 2 && flush != Z_FINISH {
                        return Ok(out);
                    }
                    let gzip = header.starts_with(&[0x1f, 0x8b]);
                    *mode = if gzip { GUNZIP } else { INFLATE };
                    *inner = Some(Box::new(new_decompress(*mode, *window_bits, None)?));
                    buffered = std::mem::take(header);
                    input = &buffered;
                }
                let decoder = inner.as_mut().expect("decoder");
                loop {
                    // As node_zlib.cc: a gzip member is followed by another one unless the next byte is 0.
                    if *ended && *mode == GUNZIP && input.first().is_some_and(|byte| *byte != 0) {
                        **decoder = new_decompress(GUNZIP, *window_bits, None)?;
                        *ended = false;
                    }
                    if *ended || input.is_empty() {
                        break;
                    }
                    let status = inflate_step(decoder, &mut input, &mut out, dictionary.as_deref())?;
                    if status == Status::StreamEnd {
                        *ended = true;
                    } else if status == Status::BufError && input.is_empty() {
                        break;
                    }
                }
                *junk |= *ended && !input.is_empty();
                if !*ended {
                    loop {
                        let mut empty: &[u8] = &[];
                        let len = out.len();
                        let status = inflate_step(decoder, &mut empty, &mut out, dictionary.as_deref())?;
                        *ended |= status == Status::StreamEnd;
                        if out.len() == len || *ended {
                            break;
                        }
                    }
                }
                if flush == Z_FINISH && !*ended {
                    return Err(eof());
                }
                Ok(out)
            }
            Codec::BrotliEncode(encoder) => brotli_encode(encoder, input, flush),
            Codec::BrotliDecode { state, ended } => {
                let out = brotli_decode(state, input, ended)?;
                if flush == 2 && !*ended {
                    return Err(eof());
                }
                Ok(out)
            }
            Codec::ZstdEncode(encoder) => zstd_encode(encoder, input, flush).map_err(zstd_error),
            Codec::ZstdDecode { inner, boundary, seen } => {
                let mut out = Vec::new();
                let mut source = InBuffer::around(input);
                let mut buffer = vec![0u8; 64 * 1024];
                *seen |= !input.is_empty();
                loop {
                    let mut sink = OutBuffer::around(&mut buffer[..]);
                    let hint = inner.run(&mut source, &mut sink).map_err(zstd_error)?;
                    let written = sink.pos();
                    out.extend_from_slice(&buffer[..written]);
                    *boundary = hint == 0;
                    if source.pos() == input.len() && written < buffer.len() {
                        break;
                    }
                }
                if flush == 2 && (!*boundary || !*seen) {
                    return Err(eof());
                }
                Ok(out)
            }
        }
    }
}

fn brotli_encode(
    encoder: &mut BrotliEncoderStateStruct<StandardAlloc>,
    input: &[u8],
    flush: i32,
) -> Result<Vec<u8>, JsError> {
    let op = match flush {
        1 => BrotliEncoderOperation::BROTLI_OPERATION_FLUSH,
        2 => BrotliEncoderOperation::BROTLI_OPERATION_FINISH,
        3 => BrotliEncoderOperation::BROTLI_OPERATION_EMIT_METADATA,
        _ => BrotliEncoderOperation::BROTLI_OPERATION_PROCESS,
    };
    let mut nop = |_: &mut interface::PredictionModeContextMap<interface::InputReferenceMut>,
                   _: &mut [interface::StaticCommand],
                   _: interface::InputPair,
                   _: &mut StandardAlloc| ();
    let (mut out, mut buffer) = (Vec::new(), vec![0u8; 64 * 1024]);
    let (mut available_in, mut offset_in) = (input.len(), 0);
    loop {
        let (mut available_out, mut offset_out, mut total) = (buffer.len(), 0, None);
        let ok = encoder.compress_stream(
            op,
            &mut available_in,
            input,
            &mut offset_in,
            &mut available_out,
            &mut buffer,
            &mut offset_out,
            &mut total,
            &mut nop,
        );
        if !ok {
            return Err(zerr("Compression failed", "ERR_BROTLI_COMPRESSION_FAILED", -1));
        }
        out.extend_from_slice(&buffer[..offset_out]);
        let done = available_in == 0 && !encoder.has_more_output();
        if done && (op != BrotliEncoderOperation::BROTLI_OPERATION_FINISH || encoder.is_finished()) {
            return Ok(out);
        }
        if done && offset_out == 0 && encoder.is_finished() {
            return Ok(out);
        }
    }
}

fn brotli_decode(state: &mut BrotliDecoder, input: &[u8], ended: &mut bool) -> Result<Vec<u8>, JsError> {
    let (mut out, mut buffer) = (Vec::new(), vec![0u8; 64 * 1024]);
    let (mut available_in, mut offset_in) = (input.len(), 0);
    while !*ended {
        let (mut available_out, mut offset_out, mut total) = (buffer.len(), 0, 0);
        let result = BrotliDecompressStream(
            &mut available_in,
            &mut offset_in,
            input,
            &mut available_out,
            &mut offset_out,
            &mut buffer,
            &mut total,
            state,
        );
        out.extend_from_slice(&buffer[..offset_out]);
        match result {
            BrotliResult::ResultSuccess => *ended = true,
            BrotliResult::NeedsMoreInput => break,
            BrotliResult::NeedsMoreOutput => {}
            BrotliResult::ResultFailure => {
                let name = format!("{:?}", state.error_code);
                let code = format!("ERR_{}", name.trim_start_matches("BROTLI_DECODER"));
                return Err(zerr("Decompression failed", &code, state.error_code as i32));
            }
        }
    }
    Ok(out)
}

fn zstd_encode(
    encoder: &mut zstd::stream::raw::Encoder<'static>,
    input: &[u8],
    flush: i32,
) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut source = InBuffer::around(input);
    while source.pos() < input.len() {
        let mut sink = OutBuffer::around(&mut buffer[..]);
        encoder.run(&mut source, &mut sink)?;
        let written = sink.pos();
        out.extend_from_slice(&buffer[..written]);
    }
    loop {
        let mut sink = OutBuffer::around(&mut buffer[..]);
        let remaining = match flush {
            1 => encoder.flush(&mut sink)?,
            2 => encoder.finish(&mut sink, true)?,
            _ => 0,
        };
        let written = sink.pos();
        out.extend_from_slice(&buffer[..written]);
        if remaining == 0 {
            return Ok(out);
        }
    }
}

fn settings<'js>(options: &Object<'js>) -> rquickjs::Result<Settings> {
    let dictionary: Option<TypedArray<'js, u8>> = options.get("dictionary")?;
    let params: Option<Vec<Vec<f64>>> = options.get("params")?;
    let params = params.unwrap_or_default().into_iter().filter(|pair| pair.len() == 2);
    Ok(Settings {
        level: options.get::<_, Option<i32>>("level")?.unwrap_or(-1),
        window_bits: options.get::<_, Option<u8>>("windowBits")?.unwrap_or(15),
        dictionary: dictionary.map(|array| bytes_of(&array)),
        params: params.map(|pair| (pair[0] as u32, pair[1] as i64)).collect(),
        pledged: options.get::<_, Option<f64>>("pledgedSrcSize")?.map(|size| size as u64),
    })
}

fn with_codec<R>(ctx: &Ctx<'_>, id: u64, f: impl FnOnce(&mut Codec) -> Result<R, JsError>) -> rquickjs::Result<R> {
    state().with_resource(id, f).and_then(|result| result).map_err(|error| throw(ctx, &error))
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "create", |ctx: Ctx<'js>, mode: u8, options: Object<'js>| {
        let codec = create(mode, settings(&options)?).map_err(|error| throw(&ctx, &error))?;
        Ok::<_, rquickjs::Error>(state().insert_resource(Box::new(codec)))
    })?;
    set_fn(&object, "process", |ctx: Ctx<'js>, id: u64, input: TypedArray<'js, u8>, flush: i32| {
        let bytes = bytes_of(&input);
        let out = with_codec(&ctx, id, |codec| codec.process(&bytes, flush))?;
        bytes_to_js(&ctx, out)
    })?;
    set_fn(&object, "trailingJunk", |ctx: Ctx<'js>, id: u64| {
        with_codec(&ctx, id, |codec| Ok(matches!(codec, Codec::Inflate { junk: true, .. })))
    })?;
    set_fn(&object, "params", |ctx: Ctx<'js>, id: u64, value: i32| {
        with_codec(&ctx, id, |codec| match codec {
            Codec::Deflate(compress) => {
                compress.set_level(level(value)).map_err(|_| zerr("Failed to set parameters", "Z_STREAM_ERROR", -2))
            }
            _ => Ok(()),
        })
    })?;
    set_fn(&object, "close", |id: u64| state().resources.borrow_mut().remove(&id).is_some())?;
    set_fn(&object, "crc32", |input: TypedArray<'js, u8>, initial: u32| {
        let mut hasher = crc32fast::Hasher::new_with_initial(initial);
        hasher.update(&bytes_of(&input));
        f64::from(hasher.finalize())
    })?;
    Ok(object)
}
