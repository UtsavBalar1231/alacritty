#![allow(dead_code)]

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TabId(u64);

impl TabId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TerminalId(u64);

impl TerminalId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Tab<T> {
    id: TabId,
    value: T,
}

impl<T> Tab<T> {
    pub fn id(&self) -> TabId {
        self.id
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    pub fn into_inner(self) -> T {
        self.value
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabSelection {
    Id(TabId),
    Index(usize),
    Next,
    Previous,
    First,
    Last,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabError {
    Empty,
    UnknownTab(TabId),
    InvalidIndex { index: usize, len: usize },
}

#[derive(Debug, Eq, PartialEq)]
pub struct CloseOutcome<T> {
    pub closed: Tab<T>,
    pub next_active: Option<TabId>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TabManager<T> {
    tabs: Vec<Tab<T>>,
    active: Option<usize>,
    next_id: u64,
}

impl<T> Default for TabManager<T> {
    fn default() -> Self {
        Self { tabs: Vec::new(), active: None, next_id: 0 }
    }
}

impl<T> TabManager<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn active_id(&self) -> Option<TabId> {
        self.active().map(Tab::id)
    }

    pub fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub fn active(&self) -> Option<&Tab<T>> {
        self.active.and_then(|index| self.tabs.get(index))
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab<T>> {
        self.active.and_then(|index| self.tabs.get_mut(index))
    }

    pub fn get(&self, id: TabId) -> Option<&Tab<T>> {
        self.index_of(id).map(|index| &self.tabs[index])
    }

    pub fn get_mut(&mut self, id: TabId) -> Option<&mut Tab<T>> {
        self.index_of(id).map(|index| &mut self.tabs[index])
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Tab<T>> {
        self.tabs.iter()
    }

    pub fn iter_mut(&mut self) -> impl ExactSizeIterator<Item = &mut Tab<T>> {
        self.tabs.iter_mut()
    }

    pub fn open(&mut self, value: T) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        self.open_with_id(id, value)
    }

    pub fn open_with_id(&mut self, id: TabId, value: T) -> TabId {
        self.next_id = self.next_id.max(id.0 + 1);
        let index = self.active.map_or(self.tabs.len(), |active| active + 1);
        self.tabs.insert(index, Tab { id, value });
        self.active = Some(index);

        id
    }

    pub fn select(&mut self, selection: TabSelection) -> Result<TabId, TabError> {
        let index = match selection {
            TabSelection::Id(id) => self.index_of(id).ok_or(TabError::UnknownTab(id))?,
            TabSelection::Index(index) => {
                if index >= self.tabs.len() {
                    return Err(TabError::InvalidIndex { index, len: self.tabs.len() });
                }
                index
            },
            TabSelection::Next => self.next_index(1)?,
            TabSelection::Previous => self.next_index(self.tabs.len().saturating_sub(1))?,
            TabSelection::First => self.non_empty_index(0)?,
            TabSelection::Last => self.non_empty_index(self.tabs.len().saturating_sub(1))?,
        };

        self.active = Some(index);
        Ok(self.tabs[index].id)
    }

    pub fn close(&mut self, id: TabId) -> Result<CloseOutcome<T>, TabError> {
        let index = self.index_of(id).ok_or(TabError::UnknownTab(id))?;
        Ok(self.close_index(index))
    }

    pub fn close_active(&mut self) -> Result<CloseOutcome<T>, TabError> {
        let index = self.active.ok_or(TabError::Empty)?;
        Ok(self.close_index(index))
    }

    fn close_index(&mut self, index: usize) -> CloseOutcome<T> {
        let closed = self.tabs.remove(index);

        if let Some(active) = self.active {
            self.active = match self.tabs.len() {
                0 => None,
                len if active == index && index < len => Some(index),
                _ if active == index => Some(index - 1),
                _ if index < active => Some(active - 1),
                _ => Some(active),
            };
        }

        CloseOutcome { closed, next_active: self.active_id() }
    }

    fn index_of(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn non_empty_index(&self, index: usize) -> Result<usize, TabError> {
        if self.tabs.is_empty() { Err(TabError::Empty) } else { Ok(index) }
    }

    fn next_index(&self, offset: usize) -> Result<usize, TabError> {
        let active = self.active.ok_or(TabError::Empty)?;
        Ok((active + offset) % self.tabs.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manager_has_no_active_tab() {
        let manager = TabManager::<u8>::new();

        assert!(manager.is_empty());
        assert_eq!(manager.len(), 0);
        assert_eq!(manager.active_id(), None);
        assert_eq!(manager.active_index(), None);
    }

    #[test]
    fn selecting_or_closing_empty_manager_fails() {
        let mut manager = TabManager::<u8>::new();

        assert_eq!(manager.select(TabSelection::Next), Err(TabError::Empty));
        assert_eq!(manager.close_active(), Err(TabError::Empty));
    }

    #[test]
    fn opening_first_tab_makes_it_active() {
        let mut manager = TabManager::new();

        let id = manager.open("first");

        assert_eq!(manager.active_id(), Some(id));
        assert_eq!(manager.active().map(Tab::value), Some(&"first"));
    }

    #[test]
    fn opening_tab_inserts_after_active_and_selects_it() {
        let mut manager = TabManager::new();
        let first = manager.open("first");
        let second = manager.open("second");
        manager.select(TabSelection::Id(first)).unwrap();
        let third = manager.open("third");

        let ids = manager.iter().map(Tab::id).collect::<Vec<_>>();

        assert_eq!(ids, vec![first, third, second]);
        assert_eq!(manager.active_id(), Some(third));
    }

    #[test]
    fn selecting_by_id_and_index_works() {
        let mut manager = TabManager::new();
        let first = manager.open(1);
        let second = manager.open(2);

        assert_eq!(manager.select(TabSelection::Id(first)), Ok(first));
        assert_eq!(manager.active().map(Tab::value), Some(&1));
        assert_eq!(manager.select(TabSelection::Index(1)), Ok(second));
        assert_eq!(manager.active().map(Tab::value), Some(&2));
        assert_eq!(
            manager.select(TabSelection::Index(2)),
            Err(TabError::InvalidIndex { index: 2, len: 2 }),
        );
    }

    #[test]
    fn next_previous_and_last_wrap() {
        let mut manager = TabManager::new();
        let first = manager.open(1);
        let second = manager.open(2);
        let third = manager.open(3);

        assert_eq!(manager.select(TabSelection::First), Ok(first));
        assert_eq!(manager.select(TabSelection::Previous), Ok(third));
        assert_eq!(manager.select(TabSelection::Next), Ok(first));
        assert_eq!(manager.select(TabSelection::Last), Ok(third));
        assert_eq!(manager.select(TabSelection::Next), Ok(first));
        assert_eq!(manager.select(TabSelection::Index(1)), Ok(second));
    }

    #[test]
    fn closing_inactive_tab_before_active_preserves_active_id() {
        let mut manager = TabManager::new();
        let first = manager.open("first");
        let second = manager.open("second");
        let third = manager.open("third");
        manager.select(TabSelection::Id(third)).unwrap();

        let outcome = manager.close(first).unwrap();

        assert_eq!(outcome.closed.into_inner(), "first");
        assert_eq!(outcome.next_active, Some(third));
        assert_eq!(manager.active_id(), Some(third));
        assert_eq!(manager.iter().map(Tab::id).collect::<Vec<_>>(), vec![second, third]);
    }

    #[test]
    fn closing_inactive_tab_after_active_preserves_active_id() {
        let mut manager = TabManager::new();
        let first = manager.open("first");
        let second = manager.open("second");
        manager.select(TabSelection::Id(first)).unwrap();

        let outcome = manager.close(second).unwrap();

        assert_eq!(outcome.closed.into_inner(), "second");
        assert_eq!(outcome.next_active, Some(first));
        assert_eq!(manager.active_id(), Some(first));
    }

    #[test]
    fn closing_active_middle_tab_selects_right_neighbor() {
        let mut manager = TabManager::new();
        let _first = manager.open("first");
        let second = manager.open("second");
        let third = manager.open("third");
        manager.select(TabSelection::Id(second)).unwrap();

        let outcome = manager.close_active().unwrap();

        assert_eq!(outcome.closed.into_inner(), "second");
        assert_eq!(outcome.next_active, Some(third));
        assert_eq!(manager.active_id(), Some(third));
    }

    #[test]
    fn closing_active_last_tab_selects_left_neighbor() {
        let mut manager = TabManager::new();
        let first = manager.open("first");
        let _second = manager.open("second");

        let outcome = manager.close_active().unwrap();

        assert_eq!(outcome.closed.into_inner(), "second");
        assert_eq!(outcome.next_active, Some(first));
        assert_eq!(manager.active_id(), Some(first));
    }

    #[test]
    fn closing_only_tab_empties_manager() {
        let mut manager = TabManager::new();
        manager.open("only");

        let outcome = manager.close_active().unwrap();

        assert_eq!(outcome.closed.into_inner(), "only");
        assert_eq!(outcome.next_active, None);
        assert!(manager.is_empty());
        assert_eq!(manager.active_id(), None);
    }

    #[test]
    fn tab_ids_are_not_reused_after_close() {
        let mut manager = TabManager::new();
        let first = manager.open(1);
        manager.close(first).unwrap();
        let second = manager.open(2);

        assert_ne!(first, second);
    }

    #[test]
    fn open_with_id_uses_provided_identifier() {
        let mut manager = TabManager::new();
        let id = TabId::new(7);

        let opened = manager.open_with_id(id, "tab");

        assert_eq!(opened, id);
        assert_eq!(manager.active_id(), Some(id));
        assert_eq!(manager.iter().map(Tab::id).collect::<Vec<_>>(), vec![id]);
        assert_eq!(manager.open("next"), TabId::new(8));
    }
}
