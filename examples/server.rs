use std::time::Duration;

use clap::Parser;
use paper_experiments_2k23::types::{BufferType::*, FrameData, Stat::*};
use remotia::capture::y4m::Y4MFrameCapturer;
use remotia::profilation::loggers::console::ConsoleAverageStatsLogger;
use remotia::profilation::time::diff::TimestampDiffCalculator;
use remotia::serialization::bincode::BincodeSerializer;
use remotia::{
    buffers::pool_registry::PoolRegistry,
    pipeline::{component::Component, registry::PipelineRegistry, Pipeline},
    processors::{error_switch::OnErrorSwitch, functional::Function, ticker::Ticker},
    profilation::time::add::TimestampAdder,
};
use remotia_ffmpeg_codecs::encoders::fillers::yuv420p::YUV420PFrameFiller;
use remotia_ffmpeg_codecs::{
    encoders::EncoderBuilder, ffi, options::Options, scaling::ScalerBuilder,
};
use remotia_srt::{
    sender::SRTFrameSender,
    srt_tokio::{options::ByteCount, SrtSocket},
};

use remotia::register;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    file_path: String,

    #[arg(short, long, default_value_t = 60)]
    framerate: u64,

    #[arg(long, default_value_t=String::from(":9000"))]
    listen_address: String,

    #[arg(long, default_value_t=String::from("libx264"))]
    codec_id: String,

    #[arg(long)]
    width: u32,

    #[arg(long)]
    height: u32,

    #[arg(id = "codec-option", long)]
    codec_options: Vec<String>,
}

#[derive(PartialEq, Eq, Hash)]
enum Pipelines {
    Main,
    Error,
}

const POOLS_SIZE: usize = 1;

#[tokio::main]
async fn main() {
    env_logger::init();

    let args = Args::parse();

    let width = args.width;
    let height = args.height;

    let mut pools = PoolRegistry::new();
    let pixels_count = (width * height) as usize;
    pools
        .register(YUVFrameBuffer, POOLS_SIZE, pixels_count * 4)
        .await;
    pools
        .register(EncodedFrameBuffer, POOLS_SIZE, pixels_count * 4)
        .await;
    pools
        .register(SerializedFrameData, POOLS_SIZE, pixels_count * 4)
        .await;

    log::info!("{:?}", args.codec_options);
    let mut options = Options::new();
    for option in args.codec_options {
        let mut fragments = option.split(" ");
        let (key, value) = (fragments.next().unwrap(), fragments.next().unwrap());
        options = options.set(key, value);
    }
    let (encoder_pusher, encoder_puller) = EncoderBuilder::new()
        .codec_id(&args.codec_id)
        .filler(YUV420PFrameFiller::new(YUVFrameBuffer))
        .encoded_buffer_key(EncodedFrameBuffer)
        .scaler(
            ScalerBuilder::new()
                .input_width(width as i32)
                .input_height(height as i32)
                .output_width(width as i32)
                .output_height(height as i32)
                .input_pixel_format(ffi::AVPixelFormat_AV_PIX_FMT_YUV420P)
                // .input_pixel_format(ffi::AVPixelFormat_AV_PIX_FMT_RGBA)
                .output_pixel_format(ffi::AVPixelFormat_AV_PIX_FMT_YUV420P)
                .build(),
        )
        .options(options)
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
                .append(pools.get(YUVFrameBuffer).redeemer().soft())
                .append(pools.get(EncodedFrameBuffer).redeemer().soft()),
        )
        .feedable()
    );

    log::info!("Waiting for connection...");
    let socket = SrtSocket::builder()
        .latency(Duration::from_millis(50))
        .set(|options| options.sender.buffer_size = ByteCount(1024 * 1024))
        .listen_on(args.listen_address.as_str())
        .await
        .unwrap();

    register!(
        pipelines,
        Pipelines::Main,
        Pipeline::<FrameData>::new()
            .link(
                Component::new()
                    .append(Ticker::new(1000 / args.framerate))
                    .append(pools.get(YUVFrameBuffer).borrower())
                    .append(TimestampAdder::new(CaptureTime))
                    .append(Y4MFrameCapturer::new(YUVFrameBuffer, &args.file_path))
                    .append(TimestampAdder::new(EncodePushTime))
                    .append(encoder_pusher),
            )
            .link(
                Component::new()
                    .append(pools.get(YUVFrameBuffer).redeemer())
                    .append(pools.get(EncodedFrameBuffer).borrower())
                    .append(encoder_puller)
                    .append(TimestampDiffCalculator::new(EncodePushTime, EncodeTime))
                    .append(OnErrorSwitch::new(pipelines.get_mut(&Pipelines::Error))),
            )
            .link(
                Component::new()
                    .append(TimestampAdder::new(TransmissionStartTime))
                    .append(pools.get(SerializedFrameData).borrower())
                    .append(BincodeSerializer::new(SerializedFrameData))
                    .append(pools.get(EncodedFrameBuffer).redeemer())
                    .append(SRTFrameSender::new(SerializedFrameData, socket))
                    .append(TimestampDiffCalculator::new(
                        TransmissionStartTime,
                        TransmissionTime,
                    ))
                    .append(pools.get(SerializedFrameData).redeemer()),
            )
            .link(
                Component::new().append(
                    ConsoleAverageStatsLogger::new()
                        .header("Statistics")
                        .log(EncodeTime)
                        .log(TransmissionTime),
                ),
            )
    );

    pipelines.run().await;
}
