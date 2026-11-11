//! Settings → Usage and cost: what the AI key has spent, by day and by
//! feature, the latest calls, the balance where the provider tells it, and
//! the prices the money is worked out at (`app::usage`).

use super::*;
use crate::UsageMsg;
use crate::app::usage::{self, Balance, PriceField, Totals};
use crate::llm;
use iced::widget::{column, row};

/// The height of the tallest bar of the days' chart.
const CHART_H: f32 = 64.0;

pub(crate) fn usage_page(app: &App) -> Element<'_, Message> {
    let mut groups = vec![balance_group(app)];
    match app.usage.entries.as_deref() {
        None => groups.push(form::group(
            tr!("Spent"),
            None,
            None,
            vec![form::item(
                tr!("This month"),
                None,
                text(tr!("Loading…")).size(ts::BODY).color(theme::dim()),
            )],
        )),
        Some(entries) => {
            let s = usage::summarize(entries, usage::now_secs());
            groups.push(spent_group(&s));
            if !s.by_purpose.is_empty() {
                groups.push(form::group(
                    tr!("This month by feature"),
                    None,
                    None,
                    s.by_purpose
                        .iter()
                        .map(|(purpose, t)| {
                            form::item(
                                usage::purpose_label(purpose),
                                Some(calls_line(t)),
                                cost_text(t),
                            )
                        })
                        .collect(),
                ));
            }
            if !s.recent.is_empty() {
                let now = usage::now_secs() as i64;
                let fine = s
                    .recent
                    .iter()
                    .filter_map(usage::Entry::cost)
                    .any(|c| c > 0.0 && c < 0.01);
                groups.push(form::group(
                    tr!("Latest calls"),
                    None,
                    None,
                    s.recent
                        .iter()
                        .map(|e| {
                            // The model said only where it is not the one in
                            // use: on every row, the same name was noise.
                            let about = if e.model == app.usage.model {
                                tr!(
                                    "{date}: {input} tokens in, {output} out",
                                    date = when(e.at as i64, now),
                                    input = usage::tokens(e.input),
                                    output = usage::tokens(e.output)
                                )
                            } else {
                                tr!(
                                    "{date}, {model}: {input} tokens in, {output} out",
                                    date = when(e.at as i64, now),
                                    model = e.model.as_str(),
                                    input = usage::tokens(e.input),
                                    output = usage::tokens(e.output)
                                )
                            };
                            let cost = e.cost().map_or_else(
                                || "—".to_string(),
                                |c| usage::money_in_column(c, fine),
                            );
                            form::item(
                                usage::call_label(&e.purpose),
                                Some(about),
                                text(cost).size(ts::BODY).color(theme::fg()),
                            )
                        })
                        .collect(),
                ));
            }
        }
    }
    groups.push(prices_group(app, &app.usage.model));
    // What spends without a press each time: said, and the reader's to
    // turn off. A file changed on disk explained itself again, and opening
    // Ask wrote its questions, with nothing anywhere saying so.
    groups.push(form::group(
        tr!("Automatic AI calls"),
        None,
        None,
        vec![form::toggle(
            tr!("Keep explanations up to date"),
            Some(
                tr!("When code changes, Clew explains the changed parts again, a few AI calls each time. When off, the status bar says the explanations are out of date, and you choose when to update them.")
                    .to_string(),
            ),
            app.keep_explanations_fresh,
            |on| Message::Usage(UsageMsg::KeepFresh(on)),
        ), form::toggle(
            tr!("Suggest questions in Ask"),
            Some(
                tr!("When Ask opens, Clew writes a few questions about the code you're on: one small AI call for each new place. When off, Ask offers general questions.")
                    .to_string(),
            ),
            app.suggest_questions,
            |on| Message::Usage(UsageMsg::SuggestQuestions(on)),
        )],
    ));
    form::page(groups)
}

/// What is left on the account: read from the provider where it tells, a
/// way to its billing page where it does not.
fn balance_group(app: &App) -> Element<'_, Message> {
    let kind = app.usage.provider.unwrap_or(llm::Provider::Anthropic);
    let provider = crate::i18n::lookup(kind.label());
    let refresh = |busy: bool| -> Element<'_, Message> {
        super::toolbar::busy_action(
            Glyph::Refresh,
            tr!("Check again"),
            Some(Message::Usage(UsageMsg::RefreshBalance)),
            busy,
        )
    };
    let row: Element<'_, Message> = if !app.usage.tells_balance {
        let about = match kind {
            llm::Provider::Custom => tr!("This endpoint doesn't report a balance.").to_string(),
            _ => tr!(
                "{provider} doesn't share the balance with API keys. See it on the billing page.",
                provider = provider
            ),
        };
        let control: Element<'_, Message> = match usage::billing_page(kind) {
            Some(_) => form::action(
                tr!("Open billing page"),
                Some(Message::Usage(UsageMsg::OpenBilling)),
            )
            .into(),
            None => space().width(0).into(),
        };
        form::item(tr!("Balance"), Some(about), control)
    } else if !app.llm_available {
        form::item(
            tr!("Balance"),
            Some(tr!("Add an API key to see it.").into()),
            space().width(0),
        )
    } else {
        let told = tr!(
            "From your {provider} account. Checking it costs nothing.",
            provider = provider
        );
        match &app.usage.balance {
            Balance::Read(amounts, _) => form::item(
                tr!("Balance"),
                Some(told),
                row![
                    text(balance_said(amounts))
                        .size(ts::BASE)
                        .font(crate::fonts::UI_MEDIUM)
                        .color(theme::fg()),
                    refresh(false),
                ]
                .spacing(8)
                .align_y(iced::Center),
            ),
            Balance::Failed(e) => form::item(
                tr!("Balance"),
                Some(tr!(
                    "Couldn't check it: {error}",
                    error = crate::app::tasks::ai_reason(e)
                )),
                refresh(false),
            ),
            Balance::Reading | Balance::Unread | Balance::NotTold => form::item(
                tr!("Balance"),
                Some(told),
                row![
                    text(tr!("Checking…")).size(ts::BODY).color(theme::dim()),
                    refresh(true),
                ]
                .spacing(8)
                .align_y(iced::Center),
            ),
        }
    };
    form::group(tr!("Account"), None, None, vec![row])
}

/// What is left, as the page says it: each currency's amount, an empty
/// one left out where another has money (DeepSeek tells "$0.00" beside a
/// yuan balance).
pub(crate) fn balance_said(amounts: &[(String, String)]) -> String {
    // Held unless it reads as nothing: an amount that does not parse is shown.
    let held = |a: &str| !a.trim().parse::<f64>().is_ok_and(|n| n == 0.0);
    let any_held = amounts.iter().any(|(_, a)| held(a));
    let shown: Vec<String> = amounts
        .iter()
        .filter(|(_, a)| !any_held || held(a))
        .map(|(c, a)| amount(c, a))
        .collect();
    shown.join(", ")
}

/// An amount in its currency, as a reader writes it: CN¥110.00, $3.10 —
/// "¥" alone is yuan or yen, and sat over costs in dollars.
fn amount(currency: &str, value: &str) -> String {
    match currency {
        "CNY" => format!("CN¥{value}"),
        "USD" => format!("${value}"),
        other => format!("{value} {other}"),
    }
}

/// Today, this month, and the days before: what each spent.
fn spent_group<'a>(s: &usage::Summary) -> Element<'a, Message> {
    let mut rows = vec![
        form::item(
            tr!("Today"),
            Some(calls_line(&s.today)),
            cost_text(&s.today),
        ),
        form::item(
            tr!("This month"),
            Some(calls_line(&s.month)),
            cost_text(&s.month),
        ),
    ];
    // The last 30 days, a bar a day, today last: by cost where every call
    // has a price, else by tokens.
    let priced = s.days.iter().all(|d| d.unpriced == 0);
    let measure = |d: &Totals| {
        if priced { d.cost } else { d.tokens() as f64 }
    };
    let top = s.days.iter().map(measure).fold(0.0_f64, f64::max);
    let now = usage::now_secs() as i64;
    let last = s.days.len().saturating_sub(1);
    let bars = iced::widget::Row::with_children(s.days.iter().enumerate().map(|(i, d)| {
        let h = if top > 0.0 {
            (measure(d) / top * f64::from(CHART_H)) as f32
        } else {
            0.0
        };
        let used = d.calls > 0;
        let bar = container(space().height(h.max(if used { 2.0 } else { 1.0 })))
            .width(Fill)
            .style(move |_: &iced::Theme| container::Style {
                background: Some(
                    if used {
                        theme::accent()
                    } else {
                        theme::border()
                    }
                    .into(),
                ),
                border: iced::Border {
                    radius: 1.5.into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            });
        // Which day it is, and what it spent: a bar alone said neither.
        let day = day_said(last - i, now);
        let said = if d.calls == 0 {
            tr!("{day}: no calls", day = day.as_str())
        } else if priced {
            tr!(
                "{day}: {calls}, {cost}",
                day = day.as_str(),
                calls = calls_count(d),
                cost = usage::money(d.cost)
            )
        } else {
            tr!("{day}: {calls}", day = day.as_str(), calls = calls_line(d))
        };
        super::toolbar::tip_at(
            container(bar)
                .height(CHART_H)
                .width(Fill)
                .align_y(iced::alignment::Vertical::Bottom),
            said,
            None,
            tooltip::Position::Top,
        )
    }))
    .spacing(3)
    .width(Fill);
    // What a bar measures, said as its heading; when it runs, under it.
    let caption = |words: &str| text(words.to_string()).size(ts::SMALL).color(theme::dim());
    rows.push(form::row_of(
        column![
            caption(if priced {
                tr!("Cost per day")
            } else {
                tr!("Tokens per day")
            }),
            bars,
            // The first bar's day, said as its tooltip says it ("30 days
            // ago" was one more than the chart holds).
            row![
                caption(&day_said(last, now)),
                space().width(Fill),
                caption(tr!("Today")),
            ],
        ]
        .spacing(6),
    ));
    // Said only when it changes how the figures read: a call with no
    // price is left out of them.
    let unpriced = s.month.unpriced;
    form::group(
        tr!("Spent"),
        (unpriced > 0).then(|| {
            trn!(
                "1 call this month has no price, so it isn't counted.",
                "{n} calls this month have no price, so they aren't counted.",
                unpriced as usize
            )
        }),
        None,
        rows,
    )
}

/// Which day `back` days before today is, as the chart's tip says it:
/// "Today", "Yesterday", "Nov 9".
fn day_said(back: usize, now: i64) -> String {
    match back {
        0 => tr!("Today").to_string(),
        1 => tr!("Yesterday").to_string(),
        _ => {
            // Noon of that day on the reader's clock, as a moment: a day's
            // length off midnight, a change of clocks moved it a day.
            let offset = usage::local_secs(now) - now;
            let local_day = usage::local_secs(now).div_euclid(86_400) - back as i64;
            super::panes::short_date(local_day * 86_400 + 43_200 - offset, now)
        }
    }
}

/// When a call was made, to the minute, on the reader's clock: "Today,
/// 14:05", "Nov 10, 09:12". A date alone left a day's calls alike.
fn when(at: i64, now: i64) -> String {
    let local = usage::local_secs(at);
    let days_back = usage::local_secs(now).div_euclid(86_400) - local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let time = format!("{:02}:{:02}", secs / 3_600, secs % 3_600 / 60);
    let day = match days_back {
        0 => tr!("Today").to_string(),
        1 => tr!("Yesterday").to_string(),
        _ => super::panes::short_date(at, now),
    };
    tr!("{day}, {time}", day = day.as_str(), time = time.as_str())
}

/// "12 calls".
fn calls_count(t: &Totals) -> String {
    trn!("1 call", "{n} calls", t.calls as usize)
}

/// "12 calls, 48k tokens".
fn calls_line(t: &Totals) -> String {
    trn!(
        "1 call, {tokens} tokens",
        "{n} calls, {tokens} tokens",
        t.calls as usize,
        tokens = usage::tokens(t.tokens())
    )
}

/// What `t` cost, or its tokens where none of it has a price.
fn cost_text<'a>(t: &Totals) -> Element<'a, Message> {
    let said = if t.calls > 0 && t.unpriced == t.calls {
        "—".to_string()
    } else {
        usage::money(t.cost)
    };
    text(said)
        .size(ts::BASE)
        .font(crate::fonts::UI_MEDIUM)
        .color(theme::fg())
        .into()
}

/// The model in use's prices: said in a line where Clew knows them, with
/// a way to change them; three boxes to fill only where it doesn't, or the
/// reader asks to. Three boxes always, and a date, read as a form to fill
/// in for prices Clew already had.
fn prices_group<'a>(app: &'a App, model: &str) -> Element<'a, Message> {
    if model.is_empty() {
        return form::group(
            tr!("Prices"),
            Some(tr!("Set a model on the Language model page first.").to_string()),
            None,
            Vec::new(),
        );
    }
    let known = usage::price_of(model);
    let own = usage::own_price(model).is_some();
    let defaults = usage::builtin_price(model).is_some();
    let restore = || {
        form::action(
            tr!("Restore defaults"),
            Some(Message::Usage(UsageMsg::ResetPrice)),
        )
    };
    // Being typed in, the fields stay: once two of them read as prices
    // they are kept, and the row turned back into a line under the pointer.
    let drafted = app.usage.price_draft.is_some();
    let editing = app.usage.editing_prices || drafted;
    if let (Some(p), false) = (known, editing) {
        let said = tr!(
            "{input} in, {output} out, per million tokens. Cached input {cached}.",
            input = price_said(p.input).as_str(),
            output = price_said(p.output).as_str(),
            cached = price_said(p.cached).as_str()
        );
        let about = match (own, app.usage.price_saved) {
            (true, true) => tr!("Your prices, saved. {prices}", prices = said.as_str()),
            (true, false) => tr!("Your prices. {prices}", prices = said.as_str()),
            (false, _) => said,
        };
        let mut buttons = row![].spacing(8).align_y(iced::Center);
        if own && defaults {
            buttons = buttons.push(restore());
        }
        buttons = buttons.push(form::action(
            tr!("Edit"),
            Some(Message::Usage(UsageMsg::EditPrices(true))),
        ));
        return form::group(
            tr!("Prices"),
            None,
            None,
            vec![form::item(model.to_string(), Some(about), buttons)],
        );
    }
    let shown = app.usage.price_draft.clone().unwrap_or_else(|| {
        known.map_or_else(Default::default, |p| {
            [p.input, p.cached, p.output].map(usage::price_text)
        })
    });
    // What is wrong with a field as typed, said under its name: "abc"
    // was taken, kept nowhere, and nothing said so.
    let problem = |which: PriceField, value: &str| -> Option<String> {
        let v = value.trim();
        if v.is_empty() {
            (drafted && which != PriceField::Cached).then(|| tr!("Needed to save.").to_string())
        } else if !v.parse::<f64>().is_ok_and(|n| n.is_finite() && n >= 0.0) {
            Some(tr!("Enter a number.").to_string())
        } else {
            None
        }
    };
    let field = |label: &str, which: PriceField, value: String| {
        let wrong = problem(which, &value);
        // Dollars per million tokens, said where the figure is typed, and
        // set at the right as the page's other figures are. A field that
        // is wrong is marked, as the Connect form marks one, and says why
        // under its name in the danger colour.
        let control = row![
            text("$").size(ts::BODY).color(theme::dim()),
            form::input("0", &value)
                .on_input(move |v| Message::Usage(UsageMsg::Price(which, v)))
                .style(if wrong.is_some() {
                    theme::input_marked
                } else {
                    theme::input
                })
                .width(Length::Fixed(96.0)),
            text(tr!("per 1M tokens"))
                .size(ts::SMALL)
                .color(theme::dim()),
        ]
        .spacing(6)
        .align_y(iced::Center);
        let control = container(control)
            .width(Fill)
            .align_x(iced::alignment::Horizontal::Right);
        match wrong {
            Some(why) => form::field_wrong(label.to_string(), why, control),
            None => form::field(label.to_string(), None, control),
        }
    };
    let [input, cached, output] = shown;
    // Done leaves the editor only with prices it can keep: pressed with
    // "abc" in a field, it closed and dropped the edit without a word.
    let all_right = [
        (PriceField::Input, &input),
        (PriceField::Cached, &cached),
        (PriceField::Output, &output),
    ]
    .iter()
    .all(|(which, value)| problem(*which, value).is_none());
    let mut rows = vec![
        field(tr!("Input"), PriceField::Input, input.clone()),
        field(tr!("Cached input"), PriceField::Cached, cached.clone()),
        field(tr!("Output"), PriceField::Output, output.clone()),
    ];
    // Editing prices Clew knows: a way back to the line, and to its own.
    if known.is_some() && editing {
        let mut buttons = row![].spacing(8).align_y(iced::Center);
        if app.usage.price_saved {
            buttons = buttons.push(text(tr!("Saved.")).size(ts::SMALL).color(theme::dim()));
        }
        if own && defaults {
            buttons = buttons.push(restore());
        }
        // A price that reads as none: a way out all the same, back to the
        // prices last kept — Done waits for prices it can keep, and the
        // reader who cleared a field to give up had no way out but typing.
        if !all_right {
            buttons = buttons.push(super::toolbar::tip_at(
                form::action(
                    tr!("Cancel"),
                    Some(Message::Usage(UsageMsg::EditPrices(false))),
                ),
                tr!("Keep the prices last saved"),
                None,
                iced::widget::tooltip::Position::Top,
            ));
        }
        let done = form::primary(
            tr!("Done"),
            all_right.then_some(Message::Usage(UsageMsg::EditPrices(false))),
        );
        buttons = buttons.push(if all_right {
            done.into()
        } else {
            super::toolbar::tip_at(
                done,
                tr!("Fix the prices marked in red first"),
                None,
                iced::widget::tooltip::Position::Top,
            )
        });
        rows.push(form::row_of(
            row![space().width(Fill), buttons].align_y(iced::Center),
        ));
    }
    // Whose prices these are, said over the fields as over the line.
    let about = match known {
        None => tr!(
            "Clew doesn't know {model}'s prices. Enter them to see what calls cost.",
            model = model
        ),
        Some(_) => model.to_string(),
    };
    form::group(tr!("Prices"), Some(about), None, rows)
}

/// A price per million tokens as the line says it: "$0.30", "$0.006".
fn price_said(dollars: f64) -> String {
    let text = usage::price_text(dollars);
    // Cents kept to two places: "$0.3" read as thirty cents to no one.
    match text.split_once('.') {
        Some((_, cents)) if cents.len() == 1 => format!("${text}0"),
        Some(_) => format!("${text}"),
        None => format!("${text}.00"),
    }
}
