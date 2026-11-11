#include <cmath>
#include <algorithm>
extern "C" void evaluate(int ns,const int* offsets,const double* price,const double* pred,double ft,double st,double ex,int si,double stop,double cost,double* results) {
 for(int i=0;i<ns;i++) {
  double pos=0,pnl=0,peak=0,dd=0,biggest=0,turnover=0;bool halted=false;
  for(int k=offsets[i];k<offsets[i+1];k++) {
   double p=price[k];if(k>offsets[i])pnl+=pos*(p-price[k-1]);
   peak=std::max(peak,pnl);dd=peak-pnl;biggest=std::max(biggest,dd);if(dd>stop)halted=true;
   double a=pred[k*3],slow=pred[k*3+si],target=0;
   if(std::abs(a)>ft)target=std::copysign(70,a);
   else if(std::abs(slow)>st)target=std::copysign(70,slow);
   else if((a!=0 || slow!=0) && pos && pos*slow>ex*70)target=pos;
   double safe=std::min(70.,std::max(0.,(1000-dd-140)/(p*.045)));
   target=std::trunc(std::clamp(target,-safe,safe));
   if(halted)target=0; if(k==offsets[i+1]-1)target=pos;
   if(std::abs(target-pos)>=1){pnl-=std::abs(target-pos)*p*cost;turnover+=std::abs(target-pos);pos=target;}
  }
  results[4*i]=pnl;results[4*i+1]=biggest;results[4*i+2]=turnover;results[4*i+3]=peak;
 }
}
