import json,sys,os
nodes=[];roots=[]
for h in range(4):
 m=json.load(open(os.path.join(sys.argv[1],'model%d.json'%h)));rr=[]
 def walk(n):
  i=len(nodes);nodes.append(None)
  if 'leaf_value' in n:nodes[i]=(-1,float(n['leaf_value']),0,0)
  else:
   l=walk(n['left_child']);r=walk(n['right_child']);nodes[i]=(n['split_feature'],float(n['threshold']),l,r)
  return i
 for t in m['tree_info']:rr.append(walk(t['tree_structure']))
 roots.append(rr)
with open(sys.argv[2],'w') as f:
 f.write('// Gradient boosted forecasts trained on independent generated SPCX paths.\n#[derive(Clone,Copy)]\nstruct Node { feature:i16, value:f64, left:u32, right:u32 }\n#[rustfmt::skip]\nstatic NODES: &[Node] = &[\n')
 for a,b,c,d in nodes:f.write('Node{feature:%d,value:%.17e,left:%d,right:%d},\n'%(a,b,c,d))
 f.write('];\n#[rustfmt::skip]\nstatic ROOTS: [&[u32];4] = [\n')
 for r in roots:f.write('&['+','.join(map(str,r))+'],\n')
 f.write('];\npub fn predict(x:&[f64;44])->[f64;4] { let mut out=[0.0;4]; for h in 0..4 { for root in ROOTS[h] { let mut i=*root as usize; loop { let n=NODES[i]; if n.feature<0 {out[h]+=n.value;break;} i=if x[n.feature as usize]<=n.value {n.left} else {n.right} as usize; } } } out }\n')
print(len(nodes),[len(r) for r in roots])
