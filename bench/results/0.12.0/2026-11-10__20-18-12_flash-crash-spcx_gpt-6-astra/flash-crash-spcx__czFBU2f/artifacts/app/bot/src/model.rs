pub struct Node {left:i32,right:i32,feature:usize,value:f32}
pub struct Model {roots:Vec<usize>,nodes:Vec<Node>,base:f32}
impl Default for Model {
    fn default() -> Self {
        let b=include_bytes!("model.bin");
        fn u(b:&[u8],i:usize)->u32{u32::from_le_bytes(b[i..i+4].try_into().unwrap())}
        fn f(b:&[u8],i:usize)->f32{f32::from_le_bytes(b[i..i+4].try_into().unwrap())}
        let count=u(b,0) as usize;let base=f(b,4);let n=u(b,8) as usize;
        let roots=(0..count).map(|i|u(b,12+4*i) as usize).collect();
        let off=12+4*count;
        let nodes=(0..n).map(|i| {let p=off+16*i;Node{left:u(b,p) as i32,right:u(b,p+4) as i32,feature:u(b,p+8) as usize,value:f(b,p+12)}}).collect();
        Self{roots,nodes,base}
    }
}
impl Model {
    pub fn predict(&self,x:&[f32])->f64 {
        let mut value=self.base;
        for root in &self.roots {
            let mut i=*root;
            loop {let n=&self.nodes[i];if n.left<0 {value+=n.value;break;} i=if x[n.feature]<n.value {n.left as usize} else {n.right as usize};}
        }
        value as f64
    }
}
