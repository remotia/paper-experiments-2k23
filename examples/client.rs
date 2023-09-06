use clap::Parser;
use remotia::pipeline::registry::PipelineRegistry;
use remotia::profilation::loggers::console::ConsoleAverageStatsLogger;
use remotia::profilation::time::add::TimestampAdder;
use remotia::serialization::bincode::BincodeDeserializer;
use remotia::{
    buffers::pool_registry::PoolRegistry,
    pipeline::{component::Component, Pipeline},
    processors::{error_switch::OnErrorSwitch, functional::Function},
    profilation::time::diff::TimestampDiffCalculator,
    render::winit::WinitRenderer,
};
use remotia_ffmpeg_codecs::{decoders::DecoderBuilder, ffi, scaling::ScalerBuilder};

use remotia::register;
use remotia_srt::{
    receiver::SRTFrameReceiver,
    srt_tokio::{options::ByteCount, SrtSocket},
};

use paper_experiments_2k23::types::{BufferType::*, Error::*, FrameData, Stat::*};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    width: u32,

    #[arg(long)]
    height: u32,

    #[arg(long, default_value_t=String::from("127.0.0.1:9000"))]
    server_address: String,

    #[arg(long, default_value_t=String::from("h264"))]
    codec_id: String,
}

const POOLS_SIZE: usize = 1;

#[derive(PartialEq, Eq, Hash)]
enum Pipelines {
    Main,
    Error,
}

#[tokio::main]
async fn main() {
    env_logger::init();
    log::info!("Hello World!");

    let args = Args::parse();

    log::info!("Streaming at {}x{}", args.width, args.height);

    let pixels_count = (args.width * args.height) as usize;
    let mut pools = PoolRegistry::new();
    pools
        .register(SerializedFrameData, POOLS_SIZE, pixels_count * 4)
        .await;
    pools
        .register(DecodedRGBAFrameBuffer, POOLS_SIZE, pixels_count * 4)
        .await;

    let (decoder_pusher, decoder_puller) = DecoderBuilder::new()
        .codec_id(&args.codec_id)
        .encoded_buffer_key(EncodedFrameBuffer)
        .decoded_buffer_key(DecodedRGBAFrameBuffer)
        .scaler(
            ScalerBuilder::new()
                .input_width(args.width as i32)
                .input_height(args.height as i32)
                .input_pixel_format(ffi::AVPixelFormat_AV_PIX_FMT_YUV420P)
                .output_pixel_format(ffi::AVPixelFormat_AV_PIX_FMT_BGRA)
                .build(),
        )
        .drain_error(NoFrame)
        .codec_error(CodecError)
        .build();

    let mut pipelines = PipelineRegistry::<FrameData, Pipelines>::new();

    register!(
        pipelines,
        Pipelines::Error,
        Pipeline::<FrameData>::singleton(
            Component::new()
                .append(Function::new(|fd| {
                    log::warn!("Dropped frame");
                    Some(fd)
                }))
                .append(pools.get(SerializedFrameData).redeemer().soft())
                .append(pools.get(DecodedRGBAFrameBuffer).redeemer().soft()),
        )
        .feedable()
    );

    log::info!("Connecting...");
    let socket = SrtSocket::builder()
        .set(|options| options.receiver.buffer_size = ByteCount(1024 * 1024))
        .call(args.server_address.as_str(), None)
        .await
        .unwrap();

    register!(
        pipelines,
        Pipelines::Main,
        Pipeline::<FrameData>::new()
            .link(
                Component::new()
                    .append(pools.get(SerializedFrameData).borrower())
                    .append(SRTFrameReceiver::new(SerializedFrameData, socket))
                    .append(BincodeDeserializer::new(SerializedFrameData))
                    .append(TimestampDiffCalculator::new(CaptureTime, ReceptionDelay))
                    .append(pools.get(SerializedFrameData).redeemer())
                    .append(TimestampAdder::new(DecodePushTime))
                    .append(decoder_pusher)
                    .append(OnErrorSwitch::new(pipelines.get_mut(&Pipelines::Error))),
            )
            .link(
                Component::new()
                    .append(pools.get(DecodedRGBAFrameBuffer).borrower())
                    .append(decoder_puller)
                    .append(OnErrorSwitch::new(pipelines.get_mut(&Pipelines::Error)))
                    .append(TimestampDiffCalculator::new(DecodePushTime, DecodeTime))
                    .append(WinitRenderer::new(
                        DecodedRGBAFrameBuffer,
                        args.width,
                        args.height,
                    ))
                    .append(TimestampDiffCalculator::new(CaptureTime, FrameDelay))
                    .append(pools.get(DecodedRGBAFrameBuffer).redeemer()),
            )
            .link(
                Component::new().append(
                    ConsoleAverageStatsLogger::new()
                        .header("Statistics")
                        .log(ReceptionDelay)
                        .log(FrameDelay)
                        .log(DecodeTime),
                ),
            )
    );

    pipelines.run().await;
}
