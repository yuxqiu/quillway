//! A text box that types nothing for keys pressed with Ctrl, so Ctrl+1 runs a
//! preset without also typing "1". iced's `text_input` inserts a key's text
//! whatever the modifiers and has no key-binding hook, so this wrapper clears
//! the text before passing the key on, as the maintainers suggest
//! (iced-rs/iced#356; `docs/research/ctrl-keys-in-text-input.md`). The box's
//! own Ctrl keys (copy, cut, paste, select all, word-wise Backspace, Delete and
//! arrows) match on the key, not its text, so they still work.

use std::borrow::Cow;

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Size, Theme, Vector, keyboard};

pub fn no_ctrl_typing<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(NoCtrlTyping { content: content.into() })
}

struct NoCtrlTyping<'a, Message> {
    content: Element<'a, Message>,
}

impl<Message> Widget<Message, Theme, iced::Renderer> for NoCtrlTyping<'_, Message> {
    // Everything goes to the box itself, tree included: like `Element::map`, the
    // wrapper has no state of its own, so focus by id still finds the box (iced#2319).
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let event = without_ctrl_text(event);
        self.content.as_widget_mut().update(tree, &event, layout, cursor, renderer, clipboard, shell, viewport);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

/// A key pressed with Ctrl loses its text, by the same test the shortcuts use.
/// The `AltGr` key isn't Ctrl on Linux, so characters typed with it stay.
fn without_ctrl_text(event: &Event) -> Cow<'_, Event> {
    match event {
        Event::Keyboard(keyboard::Event::KeyPressed { modifiers, text: Some(_), .. }) if modifiers.control() => {
            let mut event = event.clone();
            if let Event::Keyboard(keyboard::Event::KeyPressed { text, .. }) = &mut event {
                *text = None;
            }
            Cow::Owned(event)
        }
        _ => Cow::Borrowed(event),
    }
}

#[cfg(test)]
mod tests {
    use iced::keyboard::key::{Code, Physical};
    use iced::keyboard::{Key, Location, Modifiers};

    use super::*;

    fn press(c: &str, modifiers: Modifiers) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Character(c.into()),
            modified_key: Key::Character(c.into()),
            physical_key: Physical::Code(Code::Digit1),
            location: Location::Standard,
            modifiers,
            text: Some(c.into()),
            repeat: false,
        })
    }

    fn text(event: &Event) -> Option<&str> {
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed { text, .. }) => text.as_deref(),
            _ => None,
        }
    }

    #[test]
    fn keys_pressed_with_ctrl_type_nothing() {
        // Digits, AZERTY's digit row, Shift+digit symbols and Cyrillic letters all keep their text under Ctrl.
        for (c, modifiers) in [
            ("1", Modifiers::CTRL),
            ("&", Modifiers::CTRL),
            ("!", Modifiers::CTRL | Modifiers::SHIFT),
            ("в", Modifiers::CTRL),
        ] {
            assert_eq!(text(&without_ctrl_text(&press(c, modifiers))), None, "{c}");
        }
    }

    #[test]
    fn other_keys_pass_through_unchanged() {
        // AltGr is a level shift on Linux, not Ctrl or Alt.
        for (c, modifiers) in [("1", Modifiers::empty()), ("@", Modifiers::empty()), ("A", Modifiers::SHIFT)] {
            assert!(matches!(without_ctrl_text(&press(c, modifiers)), Cow::Borrowed(_)), "{c}");
        }
        let modifiers = Event::Keyboard(keyboard::Event::ModifiersChanged(Modifiers::CTRL));
        assert!(matches!(without_ctrl_text(&modifiers), Cow::Borrowed(_)));
    }
}
