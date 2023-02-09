use std::io;
use std::marker::PhantomData;
use std::time::Duration;

use indicatif::style::ProgressStyle;
use indicatif::style::ProgressTracker;
use indicatif::{MultiProgress, ProgressBar};
use tracing_core::span;
use tracing_core::Subscriber;
use tracing_subscriber::fmt::format::DefaultFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::FormattedFields;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer;
use tracing_subscriber::registry::LookupSpan;

// TODO(emersonford): add support for limiting the number of concurrent progress bars
// TODO(emersonford): add support for incrementing/non-spinner progress bars (maybe with
// Span::current().increment_progress_bar() style API?)
// TODO(emersonford): make child span progress bar stuff customizable (e.g. be able to disable
// showing child progress bars below parent one, disable the child indent, etc).

#[derive(Clone)]
struct IndicatifProgressKey {
    message: String,
}

impl ProgressTracker for IndicatifProgressKey {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(self.clone())
    }

    fn tick(&mut self, _: &indicatif::ProgressState, _: std::time::Instant) {}

    fn reset(&mut self, _: &indicatif::ProgressState, _: std::time::Instant) {}

    fn write(&self, _: &indicatif::ProgressState, w: &mut dyn std::fmt::Write) {
        let _ = w.write_str(&self.message);
    }
}

// TODO(emersonford): find a cleaner way to integrate this layer with fmt::Layer.
#[derive(Clone)]
pub struct IndicatifWriter {
    progress_bars: MultiProgress,
}

impl io::Write for IndicatifWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.progress_bars.suspend(|| io::stderr().write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.progress_bars.suspend(|| io::stderr().flush())
    }
}

impl<'a> MakeWriter<'a> for IndicatifWriter {
    type Writer = IndicatifWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

pub struct IndicatifLayer<S, F = DefaultFields> {
    progress_bars: MultiProgress,
    span_field_formatter: F,
    progress_style: ProgressStyle,
    span_child_prefix_spacing: &'static str,
    span_child_prefix_symbol: &'static str,
    inner: PhantomData<S>,
}

impl<S> IndicatifLayer<S> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S> Default for IndicatifLayer<S> {
    fn default() -> Self {
        Self {
            progress_bars: MultiProgress::new(),
            span_field_formatter: DefaultFields::new(),
            progress_style: ProgressStyle::with_template(
                "{span_child_prefix}{spinner} {span_name}{{{span_fields}}}",
            )
            .unwrap(),
            span_child_prefix_spacing: "  ",
            span_child_prefix_symbol: "↳ ",
            inner: PhantomData,
        }
    }
}

impl<S, F> IndicatifLayer<S, F> {
    /// Returns the writer that should be passed into
    /// [fmt::Layer::with_writer](tracing_subscriber::fmt::Layer::with_writer).
    ///
    /// This will result in all log messages being written to stderr and ensures the printing of
    /// the log messages does not interfere with the progress bars.
    pub fn get_writer(&self) -> IndicatifWriter {
        IndicatifWriter {
            // `MultiProgress` is merely a wrapper over an `Arc`, so we can clone here.
            progress_bars: self.progress_bars.clone(),
        }
    }

    /// Specify the formatter for span fields, the result of which will be available as the
    /// progress bar template key `span_fields`.
    pub fn with_span_field_formatter<F2>(self, formatter: F2) -> IndicatifLayer<S, F2>
    where
        F2: for<'writer> FormatFields<'writer> + 'static,
    {
        IndicatifLayer {
            progress_bars: self.progress_bars,
            span_field_formatter: formatter,
            progress_style: self.progress_style,
            span_child_prefix_spacing: self.span_child_prefix_spacing,
            span_child_prefix_symbol: self.span_child_prefix_symbol,
            inner: self.inner,
        }
    }

    /// Override the style used for displayed progress bars.
    ///
    /// Two additional keys are available for the progress bar template:
    /// * `span_fields` - the formatted string of this span's fields
    /// * `span_name` - the name of the span
    /// * `span_child_prefix` - a prefix that increase in size according to the number of parents
    ///   the span has.
    pub fn with_progress_style(mut self, style: ProgressStyle) -> Self {
        self.progress_style = style;
        self
    }

    pub fn with_span_child_prefix_spacing(mut self, spacing: &'static str) -> Self {
        self.span_child_prefix_spacing = spacing;
        self
    }

    pub fn with_span_child_prefix_symbol(mut self, symbol: &'static str) -> Self {
        self.span_child_prefix_symbol = symbol;
        self
    }
}

struct IndicatifSpanContext {
    progress_bar: Option<ProgressBar>,
    span_fields_formatted: Option<String>,
    level: u16,
}

impl<S, F> layer::Layer<S> for IndicatifLayer<S, F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    F: for<'writer> FormatFields<'writer> + 'static,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx
            .span(id)
            .expect("Span not found in context, this is a bug");
        let mut ext = span.extensions_mut();

        let mut fields = FormattedFields::<F>::new(String::new());
        let _ = self
            .span_field_formatter
            .format_fields(fields.as_writer(), attrs);

        ext.insert(IndicatifSpanContext {
            progress_bar: None,
            span_fields_formatted: Some(fields.fields),
            level: 0,
        });
    }

    fn on_enter(&self, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx
            .span(id)
            .expect("Span not found in context, this is a bug");
        let mut ext = span.extensions_mut();

        if let Some(indicatif_ctx) = ext.get_mut::<IndicatifSpanContext>() {
            // Get the next parent span with a progress bar.
            let parent_span = ctx.span_scope(id).and_then(|scope| {
                scope.skip(1).find(|span| {
                    let ext = span.extensions();

                    ext.get::<IndicatifSpanContext>().is_some()
                })
            });
            let parent_span_ext = parent_span.as_ref().map(|span| span.extensions());
            let parent_indicatif_ctx = parent_span_ext
                .as_ref()
                .map(|ext| ext.get::<IndicatifSpanContext>().unwrap());

            let span_name = span.name().to_string();
            let span_fields_formatted = indicatif_ctx
                .span_fields_formatted
                .to_owned()
                .unwrap_or_default();

            // Start the progress bar when we enter the span for the first time.
            if indicatif_ctx.progress_bar.is_none() {
                let span_child_prefix = match parent_indicatif_ctx {
                    Some(v) => {
                        indicatif_ctx.level = v.level + 1;

                        format!(
                            "{}{}",
                            self.span_child_prefix_spacing
                                .repeat(indicatif_ctx.level.into()),
                            self.span_child_prefix_symbol
                        )
                    }
                    None => String::new(),
                };

                let pb = ProgressBar::new_spinner().with_style(
                    self.progress_style
                        .clone()
                        .with_key("span_name", IndicatifProgressKey { message: span_name })
                        .with_key(
                            "span_fields",
                            IndicatifProgressKey {
                                message: span_fields_formatted,
                            },
                        )
                        .with_key(
                            "span_child_prefix",
                            IndicatifProgressKey {
                                message: span_child_prefix,
                            },
                        ),
                );

                let pb = match parent_indicatif_ctx {
                    Some(v) => {
                        // Parent spans should always have been entered at least once, meaning if
                        // they have an `IndicatifSpanContext`, they have a progress bar. So we can
                        // unwrap safely here.
                        let parent_pb: &ProgressBar = v
                            .progress_bar
                            .as_ref()
                            .expect("Parent span should have a progress bar; this is a bug");

                        self.progress_bars.insert_after(parent_pb, pb)
                    }
                    None => self.progress_bars.add(pb),
                };

                pb.enable_steady_tick(Duration::from_millis(100));

                indicatif_ctx.progress_bar = Some(pb);
            }
        }
    }

    fn on_close(&self, id: span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx
            .span(&id)
            .expect("Span not found in context, this is a bug");
        let mut ext = span.extensions_mut();

        // Clear the progress bar only when the span has closed completely.
        if let Some(pb) = ext
            .get_mut::<IndicatifSpanContext>()
            .and_then(|indicatif_ctx| indicatif_ctx.progress_bar.take())
        {
            pb.finish_and_clear();

            self.progress_bars.remove(&pb);
        }
    }
}
