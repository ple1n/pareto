use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::Result;
use futures::SinkExt;
use iced::{
    Border, Color, Element, Event, Fill, Length, Subscription, Task, Theme, event, keyboard,
    widget::{column, container, scrollable, text, text_input},
};
use iced_exwlshell::{
    actions::RegionActionCB,
    layershell::application,
    reexport::{Anchor, BlurOption, KeyboardInteractivity, Layer, LayerSize},
    settings::LayerShellSettings,
    to_layer_message,
};

use crate::dictionary::{DefinitionContent, Dictionary, Entry, LocalizedText};

const WIDTH: i32 = 600;
const HEIGHT: i32 = 520;

pub fn run(data_dir: &Path) -> Result<()> {
    let data_dir = data_dir.to_path_buf();

    application(
        move || State::new(data_dir.clone()),
        namespace,
        update,
        view,
    )
    .layer_settings(LayerShellSettings {
        size: LayerSize::px(WIDTH as u32, HEIGHT as u32),
        anchor: Anchor::empty(),
        layer: Layer::Top,
        keyboard_interactivity: KeyboardInteractivity::OnDemand,
        blur_option: BlurOption::FullRegion,
        events_transparent: true,
        ..Default::default()
    })
    .subscription(subscription)
    .style(style)
    .run()?;

    Ok(())
}

struct State {
    data_dir: PathBuf,
    dictionary: Option<Arc<Dictionary>>,
    dictionary_loading: bool,
    query: String,
    query_input_id: iced::widget::Id,
    entries: Vec<Entry>,
    error: Option<String>,
    visible: bool,
}

impl State {
    fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            dictionary: None,
            dictionary_loading: false,
            query: String::new(),
            query_input_id: iced::widget::Id::unique(),
            entries: Vec::new(),
            error: None,
            visible: true,
        }
    }
}

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    Exit,
    QueryChanged(String),
    ClearQuery,
    DictionaryOpened(std::result::Result<LoadedDictionary, String>),
    LookupFinished(std::result::Result<Vec<Entry>, String>),
    SelectionRead(Option<String>),
    Toggle,
    IcedEvent(Event),
}

#[derive(Clone)]
struct LoadedDictionary(Arc<Dictionary>);

impl std::fmt::Debug for LoadedDictionary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LoadedDictionary")
    }
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Exit => iced::exit(),
        Message::QueryChanged(query) => {
            state.query = query;
            lookup_task(state)
        }
        Message::ClearQuery => {
            state.query.clear();
            Task::batch([lookup_task(state), focus_query_input(state)])
        }
        Message::DictionaryOpened(Ok(LoadedDictionary(dictionary))) => {
            state.dictionary = Some(dictionary);
            state.dictionary_loading = false;
            state.error = None;
            lookup_task(state)
        }
        Message::DictionaryOpened(Err(error)) => {
            state.dictionary_loading = false;
            state.entries.clear();
            state.error = Some(error);
            Task::none()
        }
        Message::LookupFinished(Ok(entries)) => {
            state.entries = entries;
            state.error = None;
            Task::none()
        }
        Message::LookupFinished(Err(error)) => {
            state.entries.clear();
            state.error = Some(error);
            Task::none()
        }
        Message::SelectionRead(selection) => {
            if let Some(selection) = selection.as_deref().and_then(usable_selection) {
                state.query = selection.to_owned();
                let id = state.query_input_id.clone();
                return Task::batch([lookup_task(state), iced::widget::operation::select_all(id)]);
            }
            Task::none()
        }
        Message::Toggle => {
            state.visible = !state.visible;
            let visible = state.visible;
            let layer = if visible {
                Layer::Top
            } else {
                Layer::Background
            };
            let keyboard = if visible {
                KeyboardInteractivity::OnDemand
            } else {
                KeyboardInteractivity::None
            };
            let mut tasks = vec![
                Task::done(Message::LayerChange(layer)),
                Task::done(Message::KeyboardInteractivityChange(keyboard)),
                Task::done(Message::SetInputRegion(input_region(visible))),
            ];
            if visible {
                tasks.push(focus_query_input(state));
            }
            Task::batch(tasks)
        }
        Message::IcedEvent(Event::Window(iced::window::Event::Opened { .. })) => {
            let mut tasks = vec![
                Task::done(Message::SetInputRegion(input_region(true))),
                focus_query_input(state),
            ];
            if state.dictionary_loading || state.dictionary.is_some() {
                return Task::batch(tasks);
            }

            state.dictionary_loading = true;
            tasks.push(dictionary_open_task(state.data_dir.clone()));
            Task::batch(tasks)
        }
        Message::IcedEvent(Event::Mouse(iced::mouse::Event::ButtonPressed(_))) => {
            refocus_query_input_if_needed(state)
        }
        Message::IcedEvent(_) => Task::none(),
        _ => Task::none(),
    }
}

fn focus_query_input(state: &State) -> Task<Message> {
    let id = state.query_input_id.clone();
    iced::widget::operation::focus(id.clone()).chain(iced::widget::operation::select_all(id))
}

fn refocus_query_input_if_needed(state: &State) -> Task<Message> {
    let id = state.query_input_id.clone();
    iced::widget::operation::is_focused(id.clone()).then(move |focused| {
        if focused {
            Task::none()
        } else {
            iced::widget::operation::focus(id.clone())
                .chain(iced::widget::operation::select_all(id.clone()))
        }
    })
}

fn lookup_task(state: &mut State) -> Task<Message> {
    if state.query.trim().is_empty() {
        state.entries.clear();
        state.error = None;
        return Task::none();
    }

    let Some(dictionary) = state.dictionary.as_ref().map(Arc::clone) else {
        return Task::none();
    };
    let query = state.query.clone();
    Task::perform(
        async move {
            dictionary
                .search(&query, 5)
                .map_err(|error| error.to_string())
        },
        Message::LookupFinished,
    )
}

fn dictionary_open_task(data_dir: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let (sender, receiver) = futures::channel::oneshot::channel();
            thread::Builder::new()
                .name("pareto-dictionary-open".into())
                .spawn(move || {
                    let result = Dictionary::open(&data_dir)
                        .map(|dictionary| LoadedDictionary(Arc::new(dictionary)))
                        .map_err(|error| error.to_string());
                    let _ = sender.send(result);
                })
                .map_err(|error| error.to_string())?;

            receiver
                .await
                .map_err(|_| "dictionary loader stopped before returning".to_owned())?
        },
        Message::DictionaryOpened,
    )
}

fn subscription(_: &State) -> Subscription<Message> {
    let shortcut = keyboard::listen().filter_map(|event| {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };

        if modifiers.command() && key.as_ref() == keyboard::Key::Character("q") {
            return Some(Message::Exit);
        }
        if key.as_ref() == keyboard::Key::Named(keyboard::key::Named::Escape) {
            return Some(Message::ClearQuery);
        }
        None
    });

    Subscription::batch([
        event::listen().map(Message::IcedEvent),
        shortcut,
        gesture_events(),
        selection_events(),
    ])
}

fn selection_events() -> Subscription<Message> {
    crate::selection::subscription().map(|selection| Message::SelectionRead(Some(selection)))
}

fn usable_selection(selection: &str) -> Option<&str> {
    let selection = selection.trim();
    (!selection.is_empty() && selection.len() < 20).then_some(selection)
}

#[cfg(test)]
mod tests {
    use super::usable_selection;

    #[test]
    fn trims_selected_text_and_rejects_empty_or_long_values() {
        assert_eq!(usable_selection("  word\n"), Some("word"));
        assert_eq!(usable_selection(" \t "), None);
        assert_eq!(usable_selection("01234567890123456789"), None);
    }
}

fn gesture_events() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            10,
            |mut output: futures::channel::mpsc::Sender<Message>| async move {
                thread::spawn(move || {
                    let mut reported_unavailable = false;
                    loop {
                        let socket =
                            format!("/run/pareto/gesture-{}.sock", unsafe { libc::getuid() });
                        match UnixStream::connect(&socket) {
                            Ok(stream) => {
                                reported_unavailable = false;
                                let reader = BufReader::new(stream);
                                for line in reader.lines() {
                                    match line {
                                        Ok(line) if line == "toggle" => {
                                            if futures::executor::block_on(
                                                output.send(Message::Toggle),
                                            )
                                            .is_err()
                                            {
                                                return;
                                            }
                                        }
                                        Ok(_) => {}
                                        Err(error) => {
                                            eprintln!("gesture socket read failed: {error}");
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                if !reported_unavailable {
                                    eprintln!("gesture daemon is not available: {error}");
                                    reported_unavailable = true;
                                }
                            }
                        }
                        thread::sleep(Duration::from_millis(500));
                    }
                });
                futures::future::pending::<()>().await;
            },
        )
    })
}

fn input_region(visible: bool) -> RegionActionCB {
    RegionActionCB::new(move |region| {
        if visible {
            let radius = 22_i32.min(WIDTH / 2).min(HEIGHT / 2);
            for y in 0..HEIGHT {
                let inset = rounded_corner_inset(y, WIDTH, HEIGHT, radius);
                let span = WIDTH - 2 * inset;
                if span > 0 {
                    let _ = region.add(inset, y, span, 1);
                }
            }
        }
    })
}

fn rounded_corner_inset(y: i32, width: i32, height: i32, radius: i32) -> i32 {
    if radius == 0 || (y >= radius && y < height - radius) {
        return 0;
    }

    let row_center = if y < radius {
        y as f64 + 0.5
    } else {
        height as f64 - y as f64 - 0.5
    };
    let distance = radius as f64 - row_center;
    let remaining = (radius as f64).powi(2) - distance.powi(2);
    (radius as f64 - remaining.max(0.0).sqrt())
        .ceil()
        .clamp(0.0, (width / 2) as f64) as i32
}

fn view(state: &State) -> Element<'_, Message> {
    let results = if state.dictionary_loading {
        column![text("Loading dictionary...").size(14)]
    } else if let Some(error) = &state.error {
        column![text(error).size(14)]
    } else {
        group_entries(&state.entries)
            .into_iter()
            .fold(column![], |items, group| {
                let definitions = group
                    .definitions
                    .iter()
                    .fold(column![], |items, definition| {
                        push_definition(items, definition, true)
                    })
                    .width(Fill);

                items.push(
                    column![
                        text(group.word)
                            .size(22)
                            .color(Color::from_rgb(0.55, 0.78, 1.0)),
                        definitions.spacing(7),
                    ]
                    .width(Fill)
                    .spacing(5),
                )
            })
    };
    let results = results.width(Fill);

    container(
        column![
            text_input("Select text or type a word", &state.query)
                .id(state.query_input_id.clone())
                .on_input(Message::QueryChanged)
                .padding(12)
                .size(16)
                .style(|_, _| {
                    iced::widget::text_input::Style {
                        background: Color::from_rgba(0.08, 0.12, 0.2, 0.3).into(),
                        border: Border {
                            color: Color::from_rgba(0.72, 0.82, 1.0, 0.2),
                            width: 1.0,
                            radius: 12.0.into(),
                        },
                        placeholder: Color::from_rgb(0.55, 0.62, 0.73),
                        value: Color::WHITE,
                        icon: Color::WHITE,
                        selection: Color::from_rgb(0.28, 0.48, 0.78),
                    }
                }),
            scrollable(results).width(Fill).height(Length::Fill),
        ]
        .width(Fill)
        .spacing(14)
        .padding(20),
    )
    .width(Fill)
    .height(Fill)
    .padding(22)
    .style(|_| {
        container::Style::default()
            .background(Color::from_rgba(0.035, 0.055, 0.09, 0.05))
            .border(Border {
                color: Color::from_rgba(0.72, 0.82, 1.0, 0.25),
                width: 1.0,
                radius: 22.0.into(),
            })
    })
    .into()
}

struct WordGroup<'a> {
    word: &'a str,
    definitions: Vec<&'a DefinitionContent>,
}

fn group_entries(entries: &[Entry]) -> Vec<WordGroup<'_>> {
    let mut groups: Vec<WordGroup<'_>> = Vec::new();
    for entry in entries {
        if let Some(group) = groups.iter_mut().find(|group| group.word == entry.word) {
            group.definitions.extend(&entry.definitions);
        } else {
            groups.push(WordGroup {
                word: &entry.word,
                definitions: entry.definitions.iter().collect(),
            });
        }
    }
    groups
}

fn push_definition<'a>(
    mut items: iced::widget::Column<'a, Message>,
    definition: &'a DefinitionContent,
    indented: bool,
) -> iced::widget::Column<'a, Message> {
    let fields = definition_fields(definition);
    let fields: Element<'a, Message> = if indented {
        container(fields).width(Fill).padding([0.0, 12.0]).into()
    } else {
        fields.into()
    };
    items = items.push(fields);
    definition
        .children
        .iter()
        .fold(items, |items, child| push_definition(items, child, true))
}

fn definition_fields(definition: &DefinitionContent) -> iced::widget::Column<'_, Message> {
    let mut fields = column![].width(Fill).spacing(5);

    let mut metadata = iced::widget::row![].spacing(9);
    let mut has_metadata = false;
    for (value, color) in [
        (
            definition.word_type.as_deref(),
            Color::from_rgb(0.64, 0.78, 1.0),
        ),
        (
            definition.title.as_deref(),
            Color::from_rgb(0.86, 0.79, 1.0),
        ),
        (
            definition
                .t1
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
            Color::from_rgb(0.86, 0.79, 1.0),
        ),
        (
            definition
                .t2
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
            Color::from_rgb(0.86, 0.79, 1.0),
        ),
    ] {
        if let Some(value) = value {
            metadata = metadata.push(text(value.trim()).size(13).color(color));
            has_metadata = true;
        }
    }
    if has_metadata {
        fields = fields.push(metadata);
    }

    match (definition.cn.as_deref(), definition.en.as_deref()) {
        (Some(cn), Some(en)) => {
            fields = fields.push(
                iced::widget::row![
                    text(cn)
                        .width(Fill)
                        .size(16)
                        .color(Color::from_rgb(0.8, 0.9, 1.0)),
                    text(en)
                        .width(Fill)
                        .size(14)
                        .color(Color::from_rgba(0.88, 0.91, 0.97, 0.88)),
                ]
                .spacing(12),
            );
        }
        (Some(cn), None) => {
            fields = fields.push(text(cn).size(16).color(Color::from_rgb(0.8, 0.9, 1.0)));
        }
        (None, Some(en)) => {
            fields = fields.push(
                text(en)
                    .size(14)
                    .color(Color::from_rgba(0.88, 0.91, 0.97, 0.88)),
            );
        }
        (None, None) => {}
    }

    if let Some(info) = definition
        .info
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        fields = fields.push(
            text(info)
                .size(14)
                .color(Color::from_rgba(0.82, 0.87, 0.95, 0.9)),
        );
    }

    if !definition.pronunciation.is_empty() {
        fields = fields.push(
            text(format!(
                "Pronunciation: {}",
                definition.pronunciation.join("  ·  ")
            ))
            .size(13)
            .color(Color::from_rgb(0.73, 0.77, 1.0)),
        );
    }

    if let Some(section) = localized_section("Examples", &definition.examples) {
        fields = fields.push(section);
    }
    if let Some(section) = localized_section("Tips", &definition.tips) {
        fields = fields.push(section);
    }

    for (heading, values) in [
        ("Etymology", definition.etymology.as_slice()),
        ("Related", definition.related.as_slice()),
    ] {
        if !values.is_empty() {
            fields = fields.push(
                text(format!("{heading}: {}", values.join("  ·  ")))
                    .size(13)
                    .color(Color::from_rgba(0.72, 0.81, 0.93, 0.9)),
            );
        }
    }

    fields.width(Fill)
}

fn localized_section<'a>(
    heading: &'a str,
    values: &'a [LocalizedText],
) -> Option<Element<'a, Message>> {
    if values.is_empty() {
        return None;
    }

    let lines = values
        .iter()
        .filter_map(localized_line)
        .fold(column![], |lines, line| lines.push(line))
        .spacing(3);

    Some(
        column![
            text(heading)
                .size(12)
                .color(Color::from_rgb(0.58, 0.77, 1.0)),
            lines,
        ]
        .width(Fill)
        .spacing(2)
        .into(),
    )
}

fn localized_line(value: &LocalizedText) -> Option<Element<'_, Message>> {
    match (
        value.cn.as_deref(),
        value.en.as_deref(),
        value.text.as_deref(),
    ) {
        (Some(cn), Some(en), _) => Some(
            iced::widget::row![
                text(cn)
                    .width(Fill)
                    .size(13)
                    .color(Color::from_rgba(0.83, 0.89, 0.97, 0.9)),
                text(en)
                    .width(Fill)
                    .size(13)
                    .color(Color::from_rgba(0.72, 0.81, 0.92, 0.86)),
            ]
            .spacing(10)
            .into(),
        ),
        (Some(cn), None, _) => Some(
            text(cn)
                .size(13)
                .color(Color::from_rgba(0.83, 0.89, 0.97, 0.9))
                .into(),
        ),
        (None, Some(en), _) => Some(
            text(en)
                .size(13)
                .color(Color::from_rgba(0.72, 0.81, 0.92, 0.86))
                .into(),
        ),
        (_, _, Some(text_value)) => Some(
            text(text_value)
                .size(13)
                .color(Color::from_rgba(0.78, 0.85, 0.94, 0.88))
                .into(),
        ),
        (None, None, None) => None,
    }
}

#[cfg(test)]
mod render_tests {
    use super::group_entries;
    use crate::dictionary::Entry;

    #[test]
    fn groups_definitions_under_one_headword() {
        let entries = [
            Entry {
                word: "bright".into(),
                definitions: vec![Default::default()],
            },
            Entry {
                word: "bright".into(),
                definitions: vec![Default::default()],
            },
            Entry {
                word: "brighter".into(),
                definitions: vec![Default::default()],
            },
        ];

        let groups = group_entries(&entries);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].word, "bright");
        assert_eq!(groups[0].definitions.len(), 2);
        assert_eq!(groups[1].word, "brighter");
    }
}

fn style(_: &State, theme: &Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme.palette().text,
    }
}

fn namespace() -> String {
    String::from("pareto dictionary popup")
}
