use std::borrow::Cow;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chromiumoxid_types::{Method, Request};

use chromiumoxid_tmp::cdp::browser_protocol::network::LoaderId;
use chromiumoxid_tmp::cdp::browser_protocol::page::{
    EventFrameDetached, EventFrameStoppedLoading, EventLifecycleEvent,
    EventNavigatedWithinDocument, Frame as CdpFrame, FrameTree,
};
use chromiumoxid_tmp::cdp::browser_protocol::target::EventAttachedToTarget;
use chromiumoxid_tmp::cdp::js_protocol::runtime::*;
use chromiumoxid_tmp::cdp::{
    browser_protocol::page::{self, FrameId},
    js_protocol::runtime,
};
use crate::cmd::CommandChain;
use crate::error::DeadlineExceeded;
use crate::handler::REQUEST_TIMEOUT;
use serde_json::map::Entry;

/// TODO FrameId could optimized by rolling usize based id setup, or find better
/// design for tracking child/parent
#[derive(Debug)]
pub struct Frame {
    pub parent_frame: Option<FrameId>,
    pub id: FrameId,
    pub loader_id: Option<LoaderId>,
    pub url: Option<String>,
    pub child_frames: HashSet<FrameId>,
    pub name: Option<String>,
    pub lifecycle_events: HashSet<Cow<'static, str>>,
}

impl Frame {
    pub fn new(id: FrameId) -> Self {
        Self {
            parent_frame: None,
            id,
            loader_id: None,
            url: None,
            child_frames: Default::default(),
            name: None,
            lifecycle_events: Default::default(),
        }
    }

    pub fn with_parent(id: FrameId, parent: &mut Frame) -> Self {
        parent.child_frames.insert(id.clone());
        Self {
            parent_frame: Some(parent.id.clone()),
            id,
            loader_id: None,
            url: None,
            child_frames: Default::default(),
            name: None,
            lifecycle_events: Default::default(),
        }
    }

    fn navigated(&mut self, frame: &CdpFrame) {
        self.name = frame.name.clone();
        let url = if let Some(ref fragment) = frame.url_fragment {
            format!("{}{}", frame.url, fragment)
        } else {
            frame.url.clone()
        };
        self.url = Some(url);
    }

    fn navigated_within_url(&mut self, url: String) {
        self.url = Some(url)
    }

    fn on_loading_stopped(&mut self) {
        self.lifecycle_events.insert("DOMContentLoaded".into());
        self.lifecycle_events.insert("load".into());
    }
}

impl From<CdpFrame> for Frame {
    fn from(frame: CdpFrame) -> Self {
        Self {
            parent_frame: frame.parent_id.map(From::from),
            id: frame.id,
            loader_id: Some(frame.loader_id),
            url: Some(frame.url),
            child_frames: Default::default(),
            name: frame.name,
            lifecycle_events: Default::default(),
        }
    }
}

/// Maintains the state of the pages frame and listens to events produced by
/// chromium targeting the `Target`. Also listens for events that indicate that
/// a navigation was completed
#[derive(Debug)]
pub struct FrameManager {
    main_frame: Option<FrameId>,
    frames: HashMap<FrameId, Frame>,
    /// Timeout after which an anticipated event (related to navigation) doesn't
    /// arrive results in an error
    timeout: Duration,
    /// Track currently in progress navigation
    pending_navigations: VecDeque<(FrameNavigationRequest, NavigationWatcher)>,
    /// The currently ongoing navigation
    navigation: Option<(NavigationWatcher, Instant)>,
}

impl FrameManager {
    /// The commands to execute in order to initialize this framemanager
    pub fn init_commands() -> CommandChain {
        let enable = page::EnableParams::default();
        let get_tree = page::GetFrameTreeParams::default();
        let set_lifecycle = page::SetLifecycleEventsEnabledParams::new(true);
        let enable_runtime = runtime::EnableParams::default();
        CommandChain::new(vec![
            (enable.identifier(), serde_json::to_value(enable).unwrap()),
            (
                get_tree.identifier(),
                serde_json::to_value(get_tree).unwrap(),
            ),
            (
                set_lifecycle.identifier(),
                serde_json::to_value(set_lifecycle).unwrap(),
            ),
            (
                enable_runtime.identifier(),
                serde_json::to_value(enable_runtime).unwrap(),
            ),
        ])
    }

    pub fn main_frame(&self) -> Option<&Frame> {
        self.main_frame.as_ref().and_then(|id| self.frames.get(id))
    }

    pub fn frames(&self) -> impl Iterator<Item = &Frame> + '_ {
        self.frames.values()
    }

    pub fn frame(&self, id: &FrameId) -> Option<&Frame> {
        self.frames.get(id)
    }

    fn check_lifecycle(&self, watcher: &NavigationWatcher, frame: &Frame) -> bool {
        watcher
            .expected_lifecycle
            .iter()
            .all(|ev| frame.lifecycle_events.contains(ev))
            && frame
                .child_frames
                .iter()
                .filter_map(|f| self.frames.get(f))
                .all(|f| self.check_lifecycle(watcher, f))
    }

    fn check_lifecycle_complete(
        &self,
        watcher: &NavigationWatcher,
        frame: &Frame,
    ) -> Option<NavigationOk> {
        if !self.check_lifecycle(watcher, frame) {
            return None;
        }
        if frame.loader_id == watcher.loader_id && !watcher.same_document_navigation {
            return None;
        }
        if watcher.same_document_navigation {
            return Some(NavigationOk::SameDocumentNavigation(watcher.id));
        }
        if frame.loader_id != watcher.loader_id {
            return Some(NavigationOk::NewDocumentNavigation(watcher.id));
        }
        None
    }

    pub fn poll(&mut self, now: Instant) -> Option<FrameEvent> {
        if let Some((watcher, deadline)) = self.navigation.take() {
            if now > deadline {
                log::warn!("frame deadline exceeded");
                return Some(FrameEvent::NavigationResult(Err(
                    NavigationError::Timeout {
                        err: DeadlineExceeded::new(now, deadline),
                        id: watcher.id,
                    },
                )));
            }
            if let Some(frame) = self.frames.get(&watcher.frame_id) {
                if let Some(nav) = self.check_lifecycle_complete(&watcher, frame) {
                    return Some(FrameEvent::NavigationResult(Ok(nav)));
                } else {
                    self.navigation = Some((watcher, deadline));
                }
            } else {
                return Some(FrameEvent::NavigationResult(Err(
                    NavigationError::FrameNotFound {
                        frame: watcher.frame_id,
                        id: watcher.id,
                    },
                )));
            }
        } else {
            if let Some((req, watcher)) = self.pending_navigations.pop_front() {
                let deadline = Instant::now() + Duration::from_millis(REQUEST_TIMEOUT);
                self.navigation = Some((watcher, deadline));
                return Some(FrameEvent::NavigationRequest(req.id, req.req));
            }
        }
        None
    }

    /// entrypoint for page navigation
    pub fn goto(&mut self, req: FrameNavigationRequest) {
        if let Some(frame_id) = self.main_frame.clone() {
            self.navigate_frame(frame_id, req);
        }
    }

    /// Navigate a specific frame
    pub fn navigate_frame(&mut self, frame_id: FrameId, mut req: FrameNavigationRequest) {
        let loader_id = self.frames.get(&frame_id).and_then(|f| f.loader_id.clone());
        let watcher = NavigationWatcher::until_page_load(req.id, frame_id.clone(), loader_id);
        // insert the frame_id in the request if not present
        req.set_frame_id(frame_id);
        self.pending_navigations.push_back((req, watcher))
    }

    /// Fired when a frame moved to another session
    pub fn on_attached_to_target(&mut self, _event: &EventAttachedToTarget) {
        // _onFrameMoved
    }

    pub fn on_frame_tree(&mut self, frame_tree: FrameTree) {
        self.on_frame_attached(
            frame_tree.frame.id.clone(),
            frame_tree.frame.parent_id.clone().map(Into::into),
        );
        self.on_frame_navigated(frame_tree.frame);
        if let Some(children) = frame_tree.child_frames {
            for child_tree in children {
                self.on_frame_tree(child_tree);
            }
        }
    }
    pub fn on_frame_attached(&mut self, frame_id: FrameId, parent_frame_id: Option<FrameId>) {
        if self.frames.contains_key(&frame_id) {
            return;
        }
        if let Some(parent_frame_id) = parent_frame_id {
            if let Some(parent_frame) = self.frames.get_mut(&parent_frame_id) {
                let frame = Frame::with_parent(frame_id.clone(), parent_frame);
                self.frames.insert(frame_id.clone(), frame);
            }
        }
    }

    pub fn on_frame_detached(&mut self, event: &EventFrameDetached) {
        self.remove_frames_recursively(&event.frame_id);
    }

    pub fn on_frame_navigated(&mut self, frame: CdpFrame) {
        if frame.parent_id.is_some() {
            if let Some((id, mut f)) = self.frames.remove_entry(&frame.id) {
                for child in &f.child_frames {
                    self.remove_frames_recursively(child);
                }
                // this is necessary since we can't borrow mut and then remove recursively
                f.child_frames.clear();
                f.navigated(&frame);
                self.frames.insert(id, f);
            }
        } else {
            let mut f = if let Some(main) = self.main_frame.take() {
                // update main frame
                let mut main_frame = self.frames.remove(&main).expect("Main frame is tracked.");
                for child in &main_frame.child_frames {
                    self.remove_frames_recursively(child);
                }
                // this is necessary since we can't borrow mut and then remove recursively
                main_frame.child_frames.clear();
                main_frame.id = frame.id.clone();
                main_frame
            } else {
                // initial main frame navigation
                let frame = Frame::new(frame.id.clone());
                frame
            };
            f.navigated(&frame);
            self.main_frame = Some(f.id.clone());
            self.frames.insert(f.id.clone(), f);
        }
    }

    pub fn on_frame_navigated_within_document(&mut self, event: &EventNavigatedWithinDocument) {
        if let Some(frame) = self.frames.get_mut(&event.frame_id) {
            frame.navigated_within_url(event.url.clone());
        }
    }

    pub fn on_frame_stopped_loading(&mut self, event: &EventFrameStoppedLoading) {
        if let Some(frame) = self.frames.get_mut(&event.frame_id) {
            frame.on_loading_stopped();
        }
    }

    pub fn on_frame_execution_context_created(&mut self, _event: &EventExecutionContextCreated) {}

    pub fn on_frame_execution_context_destroyed(
        &mut self,
        _event: &EventExecutionContextDestroyed,
    ) {
    }

    pub fn on_execution_context_cleared(&mut self, _event: &EventExecutionContextsCleared) {}

    /// Fired for top level page lifecycle events (nav, load, paint, etc.)
    pub fn on_page_lifecycle_event(&mut self, event: &EventLifecycleEvent) {
        if let Some(frame) = self.frames.get_mut(&event.frame_id) {
            if event.name == "init" {
                frame.loader_id = Some(event.loader_id.clone());
                frame.lifecycle_events.clear();
            }
            frame.lifecycle_events.insert(event.name.clone().into());
        }
    }

    /// Detach all child frames
    fn remove_frames_recursively(&mut self, id: &FrameId) -> Option<Frame> {
        if let Some(mut frame) = self.frames.remove(id) {
            for child in &frame.child_frames {
                self.remove_frames_recursively(child);
            }
            if let Some(parent_id) = frame.parent_frame.take() {
                if let Some(parent) = self.frames.get_mut(&parent_id) {
                    parent.child_frames.remove(&frame.id);
                }
            }
            Some(frame)
        } else {
            None
        }
    }
}

impl Default for FrameManager {
    fn default() -> Self {
        FrameManager {
            main_frame: None,
            frames: Default::default(),
            timeout: Duration::from_millis(REQUEST_TIMEOUT),
            pending_navigations: Default::default(),
            navigation: None,
        }
    }
}

#[derive(Debug)]
pub enum FrameEvent {
    NavigationResult(Result<NavigationOk, NavigationError>),
    NavigationRequest(NavigationId, Request),
}

#[derive(Debug)]
pub enum NavigationError {
    Timeout {
        id: NavigationId,
        err: DeadlineExceeded,
    },
    FrameNotFound {
        id: NavigationId,
        frame: FrameId,
    },
}

impl NavigationError {
    pub fn navigation_id(&self) -> &NavigationId {
        match self {
            NavigationError::Timeout { id, .. } => id,
            NavigationError::FrameNotFound { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum NavigationOk {
    SameDocumentNavigation(NavigationId),
    NewDocumentNavigation(NavigationId),
}

impl NavigationOk {
    pub fn navigation_id(&self) -> &NavigationId {
        match self {
            NavigationOk::SameDocumentNavigation(id) => id,
            NavigationOk::NewDocumentNavigation(id) => id,
        }
    }
}

/// Tracks the progress of an issued `Page.navigate` request until completion.
#[derive(Debug)]
pub struct NavigationWatcher {
    id: NavigationId,
    expected_lifecycle: HashSet<Cow<'static, str>>,
    frame_id: FrameId,
    loader_id: Option<LoaderId>,
    /// Once we receive the response to the issued `Page.navigate` request we
    /// can detect whether we were navigating withing the same document or were
    /// navigating to a new document by checking if a loader was included in the
    /// response.
    same_document_navigation: bool,
}

impl NavigationWatcher {
    pub fn until_page_load(id: NavigationId, frame: FrameId, loader_id: Option<LoaderId>) -> Self {
        Self {
            id,
            expected_lifecycle: std::iter::once("load".into()).collect(),
            loader_id,
            frame_id: frame,
            same_document_navigation: false,
        }
    }

    /// Checks whether the navigation was completed
    pub fn is_lifecycle_complete(&self) -> bool {
        self.expected_lifecycle.is_empty()
    }

    fn on_frame_navigated_within_document(&mut self, ev: &EventNavigatedWithinDocument) {
        if self.frame_id == ev.frame_id {
            self.same_document_navigation = true;
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub struct NavigationId(pub usize);

#[derive(Debug)]
pub struct FrameNavigationRequest {
    pub id: NavigationId,
    pub req: Request,
    pub timeout: Duration,
}

impl FrameNavigationRequest {
    pub fn new(id: NavigationId, req: Request) -> Self {
        Self {
            id,
            req,
            timeout: Duration::from_millis(REQUEST_TIMEOUT),
        }
    }

    pub fn set_frame_id(&mut self, frame_id: FrameId) {
        if let Some(params) = self.req.params.as_object_mut() {
            if let Entry::Vacant(entry) = params.entry("frameId") {
                entry.insert(serde_json::Value::String(frame_id.into()));
            }
        }
    }
}
