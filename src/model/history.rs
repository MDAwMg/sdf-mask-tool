//! 全局撤销栈。命令按"执行/撤销"成对定义，栈容量 100。

use std::collections::VecDeque;

use super::doc::Doc;

pub const UNDO_LIMIT: usize = 100;

/// 一条可撤销的操作。`apply` 做，`undo` 撤销。
pub trait Command {
    fn apply(&mut self, doc: &mut Doc);
    fn undo(&mut self, doc: &mut Doc);
    /// 显示在撤销按钮上的名字
    fn label(&self) -> &str;
}

pub struct UndoStack {
    undo: VecDeque<Box<dyn Command>>,
    redo: Vec<Box<dyn Command>>,
    limit: usize,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: UNDO_LIMIT,
        }
    }
}

impl UndoStack {
    /// 执行命令并记入撤销栈。调用方**不需要**事先 apply。
    pub fn push(&mut self, mut cmd: Box<dyn Command>, doc: &mut Doc) {
        cmd.apply(doc);
        self.undo.push_back(cmd);
        while self.undo.len() > self.limit {
            self.undo.pop_front();
        }
        self.redo.clear();
    }

    pub fn undo(&mut self, doc: &mut Doc) -> bool {
        if let Some(mut cmd) = self.undo.pop_back() {
            cmd.undo(doc);
            self.redo.push(cmd);
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self, doc: &mut Doc) -> bool {
        if let Some(mut cmd) = self.redo.pop() {
            cmd.apply(doc);
            self.undo.push_back(cmd);
            true
        } else {
            false
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn len(&self) -> usize {
        self.undo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.undo.is_empty()
    }

    /// 栈里最新一条的名字，用于 UI 显示"撤销：xxx"
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|c| c.label())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|c| c.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SetName {
        layer: usize,
        from: String,
        to: String,
    }

    impl Command for SetName {
        fn apply(&mut self, doc: &mut Doc) {
            doc.layers[self.layer].name = self.to.clone();
        }
        fn undo(&mut self, doc: &mut Doc) {
            doc.layers[self.layer].name = self.from.clone();
        }
        fn label(&self) -> &str {
            "重命名图层"
        }
    }

    #[test]
    fn 撤销恢复上一状态() {
        let mut doc = Doc::new(8, 8);
        doc.layers[0].name = "图层 1".into();
        let mut s = UndoStack::default();
        s.push(
            Box::new(SetName {
                layer: 0,
                from: "图层 1".into(),
                to: "新名".into(),
            }),
            &mut doc,
        );
        assert_eq!(doc.layers[0].name, "新名", "push 应立即生效");
        assert!(s.undo(&mut doc));
        assert_eq!(doc.layers[0].name, "图层 1");
        assert!(s.redo(&mut doc));
        assert_eq!(doc.layers[0].name, "新名");
    }

    #[test]
    fn 超过容量丢弃最旧的() {
        let mut doc = Doc::new(8, 8);
        let mut s = UndoStack::default();
        for i in 0..105 {
            s.push(
                Box::new(SetName {
                    layer: 0,
                    from: format!("{i}"),
                    to: format!("{}", i + 1),
                }),
                &mut doc,
            );
        }
        assert_eq!(s.len(), 100);
    }

    #[test]
    fn 新的操作清空重做栈() {
        let mut doc = Doc::new(8, 8);
        let mut s = UndoStack::default();
        s.push(
            Box::new(SetName {
                layer: 0,
                from: "a".into(),
                to: "b".into(),
            }),
            &mut doc,
        );
        s.undo(&mut doc);
        assert!(s.can_redo());
        s.push(
            Box::new(SetName {
                layer: 0,
                from: "a".into(),
                to: "c".into(),
            }),
            &mut doc,
        );
        assert!(!s.can_redo(), "新操作后重做栈应清空");
    }

    #[test]
    fn 空栈撤销返回假() {
        let mut doc = Doc::new(8, 8);
        let mut s = UndoStack::default();
        assert!(!s.undo(&mut doc));
        assert!(!s.redo(&mut doc));
        assert!(!s.can_undo());
    }
}
