import ctypes as C
import numpy as np
lib=C.CDLL('/tmp/eval.so');arr=np.ctypeslib.ndpointer
lib.evaluate.argtypes=[C.c_int,arr(np.int32,flags='C_CONTIGUOUS'),arr(np.float64,flags='C_CONTIGUOUS'),arr(np.float64,flags='C_CONTIGUOUS'),C.c_double,C.c_double,C.c_double,C.c_int,C.c_double,C.c_double,arr(np.float64,flags='C_CONTIGUOUS')]
class Evaluator:
 def __init__(self,ps):
  self.names=[x[0] for x in ps];self.off=np.array([0]+list(np.cumsum([len(x[1]) for x in ps])),dtype=np.int32);self.prices=np.ascontiguousarray(np.concatenate([x[1] for x in ps]),dtype=np.float64);self.pred=np.ascontiguousarray(np.concatenate([x[2] for x in ps]),dtype=np.float64);self.out=np.zeros((len(ps),4))
 def __call__(self,ft=.08,st=.25,ex=-.03,si=1,stop=500,cost=.00065):
  lib.evaluate(len(self.names),self.off,self.prices,self.pred,ft,st,ex,si,stop,cost,self.out)
  return self.out[:,0].sum(),self.out[:,1].max(),self.out[:,0].min(),self.out[:,2].sum()
if __name__=='__main__':
 import pickle
 for label in ['train','validation','test','practice']:
  ps=pickle.load(open('/tmp/pred-'+label+'.pkl','rb'));e=Evaluator(ps)
  print(label,e(.05,.4,-.03,1))
