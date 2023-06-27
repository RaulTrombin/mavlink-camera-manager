use gst::prelude::*;

use anyhow::{anyhow, Context, Result};

use tokio::sync::broadcast;
use tracing::*;

use crate::stream::gst::utils::wait_for_element_state;

#[derive(Debug)]
#[allow(dead_code)]
pub struct PipelineRunner {
    pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
    start_signal_sender: broadcast::Sender<()>,
    killswitch_sender: broadcast::Sender<String>,
    _killswitch_receiver: broadcast::Receiver<String>,
    _watcher_thread_handle: std::thread::JoinHandle<()>,
}

impl PipelineRunner {
    #[instrument(level = "debug")]
    pub fn try_new(
        pipeline: &gst::Pipeline,
        pipeline_id: &uuid::Uuid,
        allow_block: bool,
    ) -> Result<Self> {
        let pipeline_weak = pipeline.downgrade();
        let (killswitch_sender, _killswitch_receiver) = broadcast::channel(1);
        let watcher_killswitch_receiver = killswitch_sender.subscribe();
        let (start_signal_sender, start_signal_receiver) = broadcast::channel(1);

        Ok(Self {
            pipeline_weak: pipeline_weak.clone(),
            start_signal_sender,
            killswitch_sender: killswitch_sender.clone(),
            _killswitch_receiver,
            _watcher_thread_handle: std::thread::Builder::new()
                .name(format!("PipelineRunner-{pipeline_id}"))
                .spawn(move || {
                    let mut reason = "Normal ending".to_string();
                    if let Err(error) = PipelineRunner::runner(
                        pipeline_weak,
                        pipeline_id,
                        watcher_killswitch_receiver,
                        start_signal_receiver,
                        allow_block,
                    ) {
                        error!("PipelineWatcher ended with error: {error}");
                        reason = error.to_string();
                    } else {
                        info!("PipelineWatcher ended with no error.");
                    }

                    // Any ending reason should interrupt the respective pipeline
                    if let Err(reason) = killswitch_sender.send(reason) {
                        error!("Failed to broadcast error from PipelineWatcher. Reason: {reason}");
                    } else {
                        info!("Error sent to killswitch channel!");
                    }
                })
                .context(format!(
                    "Failed when spawing PipelineRunner thread for Pipeline {pipeline_id:#?}"
                ))?,
        })
    }

    #[instrument(level = "debug", skip(self))]
    pub fn get_receiver(&self) -> broadcast::Receiver<String> {
        self.killswitch_sender.subscribe()
    }

    #[instrument(level = "debug", skip(self))]
    pub fn start(&self) -> Result<()> {
        self.start_signal_sender.send(())?;
        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    pub fn is_running(&self) -> bool {
        !self._watcher_thread_handle.is_finished()
    }

    #[instrument(level = "debug")]
    fn runner(
        pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
        pipeline_id: &uuid::Uuid,
        allow_block: bool,
    ) -> Result<()> {
        let pipeline = pipeline_weak
            .upgrade()
            .context("Unable to access the Pipeline from its weak reference")?;

        let bus = pipeline
            .bus()
            .context("Unable to access the pipeline bus")?;

        // Check if we need to break external loop.
        // Some cameras have a duplicated timestamp when starting.
        // to avoid restarting the camera once and once again,
        // this checks for a maximum of 10 lost before restarting.
        let mut previous_position: Option<gst::ClockTime> = None;
        let mut lost_timestamps: usize = 0;
        let max_lost_timestamps: usize = 15;

        let mut start_received = false;

        'outer: loop {
            std::thread::sleep(std::time::Duration::from_millis(100));

            // Wait the signal to start
            if !start_received {
                if let Err(error) = start_signal_receiver.try_recv() {
                    match error {
                        broadcast::error::TryRecvError::Empty => continue,
                        _ => return Err(anyhow!("Failed receiving start signal: {error:?}")),
                    }
                }
                debug!("Starting signal received in Pipeline {pipeline_id}");
                start_received = true;
            }

            if pipeline.current_state() != gst::State::Playing {
                if let Err(error) = pipeline.set_state(gst::State::Playing) {
                    return Err(anyhow!(
                        "Failed setting Pipeline {pipeline_id} to Playing state. Reason: {error:?}"
                    ));
                }
                if let Err(error) = wait_for_element_state(
                    pipeline.upcast_ref::<gst::Element>(),
                    gst::State::Playing,
                    100,
                    5,
                ) {
                    error!(
                        "Failed setting Pipeline {pipeline_id} to Playing state. Reason: {error:?}"
                    );
                    continue;
                }
            }

            'inner: loop {
                // Restart pipeline if pipeline position do not change,
                // occur if usb connection is lost and gst do not detect it
                if !allow_block {
                    if let Some(position) = pipeline.query_position::<gst::ClockTime>() {
                        previous_position = match previous_position {
                            Some(current_previous_position) => {
                                if current_previous_position.nseconds() != 0
                                    && current_previous_position == position
                                {
                                    lost_timestamps += 1;
                                    warn!("Position did not change {lost_timestamps}");
                                } else {
                                    // We are back in track, erase lost timestamps
                                    lost_timestamps = 0;
                                }

                                if lost_timestamps > max_lost_timestamps {
                                    warn!("Pipeline lost too many timestamps (max. was {max_lost_timestamps}).");
                                    lost_timestamps = 0;
                                    break 'inner;
                                }

                                Some(position)
                            }
                            None => Some(position),
                        }
                    }
                }

                /* Iterate messages on the bus until an error or EOS occurs,
                 * although in this example the only error we'll hopefully
                 * get is if the user closes the output window */
                while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::Eos(eos) => {
                            warn!("Received EndOfStream: {eos:#?}");
                            pipeline.debug_to_dot_file_with_ts(
                                gst::DebugGraphDetails::all(),
                                format!("pipeline-{pipeline_id}-eos"),
                            );
                            break 'outer;
                        }
                        MessageView::Error(error) => {
                            error!(
                                "Error from {:?}: {} ({:?})",
                                error.src().map(|s| s.path_string()),
                                error.error(),
                                error.debug()
                            );
                            pipeline.debug_to_dot_file_with_ts(
                                gst::DebugGraphDetails::all(),
                                format!("pipeline-{pipeline_id}-error"),
                            );
                            break 'inner;
                        }
                        MessageView::StateChanged(state) => {
                            pipeline.debug_to_dot_file_with_ts(
                                gst::DebugGraphDetails::all(),
                                format!(
                                    "pipeline-{pipeline_id}-{:?}-to-{:?}",
                                    state.old(),
                                    state.current()
                                ),
                            );

                            trace!(
                                "State changed from {:?}: {:?} to {:?} ({:?})",
                                state.src().map(|s| s.path_string()),
                                state.old(),
                                state.current(),
                                state.pending()
                            );
                        }
                        other_message => trace!("{other_message:#?}"),
                    };
                }

                if let Ok(reason) = killswitch_receiver.try_recv() {
                    debug!("Killswitch received as {pipeline_id:#?} from PipelineRunner's watcher. Reason: {reason:#?}");
                    break 'outer;
                }
            }
        }

        Ok(())
    }
}

impl Drop for PipelineRunner {
    #[instrument(level = "debug", skip(self))]
    fn drop(&mut self) {
        if let Some(pipeline) = self.pipeline_weak.upgrade() {
            pipeline.send_event(gst::event::Eos::new());
        }

        if let Err(reason) = self
            .killswitch_sender
            .send("PipelineRunner Dropped.".to_string())
        {
            warn!(
                "Failed to send killswitch message while Dropping PipelineRunner. Reason: {reason:#?}"
            );
        }
    }
}
