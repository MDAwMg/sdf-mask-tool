//! 图层编辑操作：复制、清空、上下移、填充、反相、删除。

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::{DEFAULT_STEPS, Doc};

fn doc_with_content() -> Doc {
    let mut d = Doc::new(16, 16);
    d.seed_default_frames();
    let mut m = Mask::new(16, 16);
    for y in 0..16 {
        for x in 0..8 {
            m.set(x, y, 1);
        }
    }
    d.active_channel_mut().insert_frame(0.0, m);
    d
}

#[test]
fn 复制图层连内容一起复制() {
    let mut d = doc_with_content();
    let before = d.layers.len();
    let new_idx = d.duplicate_layer(0).unwrap();
    assert_eq!(d.layers.len(), before + 1);
    assert_eq!(d.active_layer, new_idx, "复制后应选中新层");

    // 内容一致
    let a = &d.layers[0].channels[0];
    let b = &d.layers[new_idx].channels[0];
    assert_eq!(a.frames.len(), b.frames.len());
    assert_eq!(
        a.frames[0].mask.count(),
        b.frames[0].mask.count(),
        "复制后的遮罩内容应一致"
    );
    // 阴影应独立：改一个不影响另一个
    let mut da = d.layers[0].channels[0].frames[0].mask.clone();
    da.clear();
    d.layers[0].channels[0].frames[0].mask = da;
    assert!(d.layers[new_idx].channels[0].frames[0].mask.count() > 0);
}

#[test]
fn 复制图层名带拷贝后缀() {
    let mut d = Doc::new(8, 8);
    d.layers[0].name = "脸".into();
    d.duplicate_layer(0).unwrap();
    assert!(d.layers[1].name.contains("拷贝"));
}

#[test]
fn 清空图层只清内容保留结构() {
    let mut d = doc_with_content();
    let n_frames = d.layers[0].channels[0].frames.len();
    d.clear_layer(0);
    assert_eq!(
        d.layers[0].channels[0].frames.len(),
        n_frames,
        "帧结构应保留"
    );
    assert!(
        d.layers[0].channels[0].frames[0]
            .mask
            .data()
            .iter()
            .all(|&v| v == 0),
        "内容应被清空"
    );
}

#[test]
fn 图层上下移动() {
    let mut d = Doc::new(8, 8);
    d.add_layer("B");
    d.add_layer("C");
    // 现在是 [图层1, B, C]，选中 C(2)
    d.move_layer(2, 0);
    assert_eq!(d.layers[0].name, "C");
    assert_eq!(d.active_layer, 0);
}

#[test]
fn 移动到越界位置不生效() {
    let mut d = Doc::new(8, 8);
    let n = d.layers.len();
    d.move_layer(0, 99);
    assert_eq!(d.layers.len(), n);
    assert_eq!(d.layers[0].name, "图层 1");
}

#[test]
fn 填满本帧() {
    let mut d = Doc::new(8, 8);
    d.seed_default_frames();
    d.fill_active_frame(0, DEFAULT_STEPS, 1);
    let f = d.active_channel_ref().frame_at(0.0).unwrap();
    assert_eq!(f.mask.count(), 64);
}

#[test]
fn 反相本帧() {
    let mut d = Doc::new(4, 4);
    d.seed_default_frames();
    d.fill_active_frame(0, DEFAULT_STEPS, 1);
    d.invert_active_frame(0, DEFAULT_STEPS);
    let f = d.active_channel_ref().frame_at(0.0).unwrap();
    assert_eq!(f.mask.count(), 0, "全 1 反相后应全 0");
}

#[test]
fn 删除图层保留至少一个() {
    let mut d = Doc::new(8, 8);
    d.remove_layer(0);
    assert_eq!(d.layers.len(), 1, "不能删到零个图层");
}

#[test]
fn 删除后选中回退到有效层() {
    let mut d = Doc::new(8, 8);
    d.add_layer("B");
    assert_eq!(d.active_layer, 1);
    d.remove_layer(1);
    assert_eq!(d.active_layer, 0);
    assert!(d.active_layer < d.layers.len(), "选中下标必须有效");
}
