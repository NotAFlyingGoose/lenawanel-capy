use std::{mem, backtrace::{self, Backtrace}};

use drop_bomb::DropBomb;
use syntax::NodeKind;

use crate::event::Event;

use super::Parser;

pub(crate) struct Marker {
    pos: usize,
    bomb: DropBomb,
}

impl Marker {
    pub(crate) fn new(pos: usize) -> Self {
        Self {
            pos,
            bomb: DropBomb::new("Markers need to be completed"),
        }
    }

    pub(crate) fn complete(mut self, p: &mut Parser<'_>, kind: NodeKind) -> CompletedMarker {
        self.bomb.defuse();
        let old_event = mem::replace(&mut p.events[self.pos], Some(Event::StartNode { 
            kind 
        }));
        debug_assert!(old_event.is_none());
        p.events.push(Some(Event::FinishNode));

        CompletedMarker { pos: self.pos }
    }
}

pub(crate) struct CompletedMarker {
    pos: usize,
}

impl CompletedMarker {
    pub(crate) fn precede(self, p: &mut Parser) -> Marker {
        p.events.insert(self.pos, None);
        Marker::new(self.pos)
    }
}
