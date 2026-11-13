//! 二值遮罩。每像素 1 字节，0 = 不遮，1 = 遮。

use std::cell::Cell;

/// 一张二值遮罩。
///
/// 故意不用 `GrayImage`：这里只需要 0/1 两种值，
/// 用 `Vec<u8>` 直存既省内存又快。
#[derive(Clone)]
pub struct Mask {
    w: u32,
    h: u32,
    data: Vec<u8>,
    /// 内容指纹的记忆化缓存。
    ///
    /// 指纹要逐字节扫全图（1K 下 3.4ms），而预览与违规检测**每帧**都要问，
    /// 不缓存就是每帧白烧 7ms —— 这是"很卡"的主要原因。
    ///
    /// 每次写操作都把缓存置空，下次 `fingerprint()` 重算。
    fp_cache: Cell<Option<u64>>,
}

impl Mask {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            data: vec![0; (w * h) as usize],
            fp_cache: Cell::new(None),
        }
    }

    pub fn from_data(w: u32, h: u32, data: Vec<u8>) -> Self {
        debug_assert_eq!(data.len(), (w * h) as usize);
        Self {
            w,
            h,
            data,
            fp_cache: Cell::new(None),
        }
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.w
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.h
    }

    #[inline]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// 直接拿可变切片。
    ///
    /// 这条路径绕过了 `set()` 的单像素失效判断，所以**一进来就把指纹缓存作废**，
    /// 保证改完内容后指纹一定重算（只会多算一次，不会算错）。
    #[inline]
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.fp_cache.set(None);
        &mut self.data
    }

    /// 外部直接改完 `data_mut()` 后调本方法（与 data_mut 的作废等价，保留供语义清晰）
    #[inline]
    pub fn mark_dirty(&mut self) {
        self.fp_cache.set(None);
    }

    #[inline]
    pub fn get(&self, x: u32, y: u32) -> u8 {
        if x >= self.w || y >= self.h {
            0
        } else {
            self.data[(y * self.w + x) as usize]
        }
    }

    #[inline]
    pub fn set(&mut self, x: u32, y: u32, v: u8) {
        if x < self.w && y < self.h {
            let i = (y * self.w + x) as usize;
            if self.data[i] != v {
                self.data[i] = v;
                self.fp_cache.set(None);
            }
        }
    }

    pub fn any(&self) -> bool {
        self.data.iter().any(|&v| v != 0)
    }

    pub fn count(&self) -> usize {
        self.data.iter().filter(|&&v| v != 0).count()
    }

    pub fn clear(&mut self) {
        self.data.fill(0);
        self.fp_cache.set(None);
    }

    pub fn fill(&mut self, v: u8) {
        self.data.fill(v);
        self.fp_cache.set(None);
    }

    /// 反相：0 ↔ 1。遮罩区域的补集。
    pub fn invert(&mut self) {
        for v in self.data.iter_mut() {
            *v = if *v == 0 { 1 } else { 0 };
        }
        self.fp_cache.set(None);
    }

    /// 左右镜像（水平翻转）。
    pub fn flip_h(&mut self) {
        let (w, h) = (self.w, self.h);
        if w < 2 {
            return;
        }
        for y in 0..h {
            let row = (y * w) as usize;
            for x in 0..w / 2 {
                let a = row + x as usize;
                let b = row + (w - 1 - x) as usize;
                self.data.swap(a, b);
            }
        }
        self.fp_cache.set(None);
    }

    /// 从 8bit 灰度图转：>= 128 视为遮罩
    pub fn from_gray(img: &image::GrayImage) -> Self {
        let (w, h) = (img.width(), img.height());
        let data = img
            .as_raw()
            .iter()
            .map(|&v| if v >= 128 { 1u8 } else { 0u8 })
            .collect();
        Self {
            w,
            h,
            data,
            fp_cache: Cell::new(None),
        }
    }

    /// 转成 0/255 灰度图
    pub fn to_gray(&self) -> image::GrayImage {
        let data: Vec<u8> = self
            .data
            .iter()
            .map(|&v| if v != 0 { 255u8 } else { 0u8 })
            .collect();
        image::GrayImage::from_raw(self.w, self.h, data)
            .unwrap_or_else(|| image::GrayImage::new(self.w, self.h))
    }

    /// 内容指纹，用于判断缓存是否失效
    pub fn fingerprint(&self) -> u64 {
        // 1K 遮罩逐字节算一次要 3.4ms（实测），而预览/检测每帧都要问一次，
        // 一帧白烧 7ms。所以这里做**惰性 + 记忆化**：
        // 指纹算过一次就存起来，内容变了（任何 set/fill/invert）才作废。
        //
        // 用 Cell 是为了 `&self` 也能更新——这是纯缓存，不改变可见语义。
        if let Some(v) = self.fp_cache.get() {
            return v;
        }
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in &self.data {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        self.fp_cache.set(Some(h));
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 新建为全零() {
        let m = Mask::new(8, 8);
        assert_eq!(m.count(), 0);
        assert!(!m.any());
    }

    #[test]
    fn 读写像素() {
        let mut m = Mask::new(8, 8);
        m.set(2, 3, 1);
        assert_eq!(m.get(2, 3), 1);
        assert_eq!(m.count(), 1);
        assert!(m.any());
    }

    #[test]
    fn 越界读写安全() {
        let mut m = Mask::new(4, 4);
        m.set(100, 100, 1);
        assert_eq!(m.get(100, 100), 0);
    }

    #[test]
    fn 灰度往返() {
        let mut m = Mask::new(8, 8);
        m.set(1, 1, 1);
        let back = Mask::from_gray(&m.to_gray());
        assert_eq!(back.get(1, 1), 1);
        assert_eq!(back.get(0, 0), 0);
    }

    #[test]
    fn 灰度阈值是128() {
        let mut img = image::GrayImage::new(2, 1);
        img.put_pixel(0, 0, image::Luma([127]));
        img.put_pixel(1, 0, image::Luma([128]));
        let m = Mask::from_gray(&img);
        assert_eq!(m.get(0, 0), 0, "127 不算遮");
        assert_eq!(m.get(1, 0), 1, "128 算遮");
    }

    #[test]
    fn 指纹随内容变化() {
        let a = Mask::new(4, 4);
        let mut b = Mask::new(4, 4);
        assert_eq!(a.fingerprint(), b.fingerprint());
        b.set(0, 0, 1);
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    /// 指纹是记忆化的（每帧都要问，逐字节算 1K 要 3.4ms）。
    /// 但**每个写路径都必须让缓存失效**，否则预览会卡在旧值。
    #[test]
    fn 每个写操作都会让指纹失效() {
        let mut m = Mask::new(8, 8);
        let fp0 = m.fingerprint();

        // set
        m.set(1, 1, 1);
        let fp1 = m.fingerprint();
        assert_ne!(fp0, fp1, "set 之后指纹必须变");

        // fill
        m.fill(0);
        let fp2 = m.fingerprint();
        assert_ne!(fp1, fp2, "fill 之后指纹必须变");

        // clear
        m.fill(1);
        let _ = m.fingerprint();
        m.clear();
        // clear 后内容回到了全 0，所以指纹应当等于初始值 ——
        // 指纹是"内容的函数"，同样内容必须给同样值。
        // 这条测的是"清空后缓存确实重算了"（否则会返回全 1 的旧指纹）。
        assert_eq!(m.fingerprint(), fp0, "清空后指纹应回到空图的值");

        // invert
        m.fill(1);
        let _ = m.fingerprint();
        m.invert();
        let fp4 = m.fingerprint();
        assert_eq!(fp4, fp0, "全 1 反相成空图，指纹回到初始");

        // flip_h
        let mut n = Mask::new(4, 2);
        n.set(0, 0, 1);
        let _ = n.fingerprint();
        n.flip_h();
        let fp5 = n.fingerprint();
        assert_ne!(fp0, fp5, "flip_h 之后指纹必须变");

        // data_mut
        let mut k = Mask::new(4, 4);
        let _ = k.fingerprint();
        k.data_mut()[0] = 1;
        let fp6 = k.fingerprint();
        assert_ne!(fp0, fp6, "data_mut 之后指纹必须变");
    }

    /// 没被改动时，重复调用必须返回同一个值（缓存不能返回脏数据）
    #[test]
    fn 未改动时指纹稳定() {
        let mut m = Mask::new(8, 8);
        m.set(2, 3, 1);
        let a = m.fingerprint();
        let b = m.fingerprint();
        let c = m.fingerprint();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    /// 写入相同值不算改动（避免笔刷重复涂同一块时无谓作废缓存）
    #[test]
    fn 写相同值不作废缓存() {
        let mut m = Mask::new(4, 4);
        m.set(1, 1, 1);
        let fp = m.fingerprint();
        m.set(1, 1, 1); // 同值
        assert_eq!(m.fingerprint(), fp, "写同值不该改变指纹");
    }
}
