use std::sync::Arc;

/// Immutable frame projection. Cloning a snapshot shares material/edit state;
/// consumers generate only the current frame instead of expanding the entire plan.
pub struct FrameList<T> {
    len: usize,
    at: Arc<dyn Fn(usize) -> T + Send + Sync>,
}
impl<T: Copy + Send + Sync + 'static> FrameList<T> {
    pub fn generated(len: usize, at: impl Fn(usize) -> T + Send + Sync + 'static) -> Self {
        Self {
            len,
            at: Arc::new(at),
        }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get(&self, i: usize) -> Option<T> {
        (i < self.len).then(|| (self.at)(i))
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = T> + '_ {
        (0..self.len).map(|i| (self.at)(i))
    }
    pub fn map<U: Copy + Send + Sync + 'static>(
        &self,
        f: impl Fn(T) -> U + Send + Sync + 'static,
    ) -> FrameList<U> {
        let source = self.clone();
        FrameList::generated(self.len, move |i| f(source.get(i).unwrap()))
    }
}
impl<T> Clone for FrameList<T> {
    fn clone(&self) -> Self {
        Self {
            len: self.len,
            at: self.at.clone(),
        }
    }
}
impl<T> std::fmt::Debug for FrameList<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameList").field("len", &self.len).finish()
    }
}
impl<T: Copy + Send + Sync + 'static> From<Vec<T>> for FrameList<T> {
    fn from(items: Vec<T>) -> Self {
        let len = items.len();
        Self::generated(len, move |i| items[i])
    }
}
impl<T: Copy + Send + Sync + PartialEq + 'static> PartialEq for FrameList<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}
impl<'a, T: Copy + Send + Sync + 'static> IntoIterator for &'a FrameList<T> {
    type Item = T;
    type IntoIter = Box<dyn Iterator<Item = T> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}
impl<T: Copy + Send + Sync + 'static> FromIterator<T> for FrameList<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}
