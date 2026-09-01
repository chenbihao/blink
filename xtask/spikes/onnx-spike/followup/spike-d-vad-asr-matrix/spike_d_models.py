#!/usr/bin/env python3
"""Spike D models: EnergyVad, FSMN-VAD ONNX, ParaformerOnline, ParaformerOffline,
VAD evaluation, CER, corpus builder, combo runner.
"""
import os,sys,time,json,wave,math,traceback,gc
import numpy as np
from collections import defaultdict
sys.stderr.reconfigure(encoding="utf-8",errors="replace")
sys.stdout.reconfigure(encoding="utf-8",errors="replace")
import warnings;warnings.filterwarnings("ignore")
try: import psutil
except: psutil=None
try: import onnxruntime as ort
except: print("ERROR: onnxruntime");sys.exit(1)
try: import librosa
except: print("ERROR: librosa");sys.exit(1)

# Paths
S=os.path.dirname(os.path.abspath(__file__))
F=os.path.dirname(S);O=os.path.dirname(F)
MD=os.path.join(O,"models")
# Constants
SR=16000;FS_MS=10;FL_MS=25;NM=80;LM=7;LN=6;FD=LM*NM
CZ=[5,10,5];ES=512;FL=16;FR=10;FD2=512;CT=1.0;TA=0.45;CSS=9600
VFL=400;VFS=160;VCL=4;VCD=128;VLO=19
EVT=0.005;EVMS=300;EVMN=800

# ─── Audio utils ──────────────────────────────────────────────────────────

def lwav(p):
    w=wave.open(p,"rb");assert w.getframerate()==16000;assert w.getnchannels()==1
    return np.frombuffer(w.readframes(w.getnframes()),dtype=np.int16).astype(np.float32)/32768.0

def gtone(d,f=440,a=0.1,sr=SR):
    n=int(d*sr);t=np.arange(n)/sr
    return(np.sin(2*np.pi*f*t)*a).astype(np.float32)

def gsil(d,sr=SR):
    return np.zeros(int(d*sr),dtype=np.float32)

def gnoise(d,a=0.02,sr=SR):
    return(np.random.randn(int(d*sr))*a).astype(np.float32)

def greverb(s,dc=0.3,dm=50,sr=SR):
    d=int(dm*sr/1000);o=s.copy()
    for i in range(d,len(s)):o[i]+=dc*o[i-d]
    return o/(np.max(np.abs(o)+1e-10))*np.max(np.abs(s))

# ─── Corpus ───────────────────────────────────────────────────────────────

def build_corpus():
    np.random.seed(42);C=[]
    def add(n,a,s,c):C.append({"name":n,"audio":a,"segments":s,"conditions":c})
    add("clean_near_field",np.concatenate([gtone(2,220,.1),gsil(.5),gtone(1,330,.1)]),[(0,2),(2.5,3.5)],["安静近讲"])
    add("far_field",np.concatenate([greverb(gtone(2,220,.05),.4,80),gsil(.5),greverb(gtone(1,330,.05),.4,80)]),[(0,2),(2.5,3.5)],["远场"])
    n=gnoise(5,.03);sp=np.concatenate([gtone(2,220,.08),gsil(.5),gtone(1,330,.08)])
    if len(n)<len(sp):n=np.pad(n,(0,len(sp)-len(n)))
    else:n=n[:len(sp)]
    add("fan_ac_bg",sp+n,[(0,2),(2.5,3.5)],["风扇/空调"])
    s=gtone(2,220,.1)
    for cp in range(0,len(s),1600):
        cl=gtone(.01,2000,.15);e=min(cp+len(cl),len(s));s[cp:e]+=cl[:e-cp]
    add("keyboard",np.concatenate([s,gsil(.5),gtone(1,330,.1)]),[(0,2),(2.5,3.5)],["键盘"])
    m=gtone(5,660,.04);sp=np.concatenate([gtone(2,220,.08),gsil(.5),gtone(1,330,.08)])
    if len(m)<len(sp):m=np.pad(m,(0,len(sp)-len(m)))
    else:m=m[:len(sp)]
    add("music_bg",sp+m,[(0,2),(2.5,3.5)],["音乐/视频"])
    add("short_word",np.concatenate([gtone(.3,440,.1),gsil(.4)]),[(0,.3)],["短词"])
    add("long_sentence",np.concatenate([gtone(5,220,.1),gsil(.5)]),[(0,5)],["长句"])
    add("mid_pause",np.concatenate([gtone(1,220,.1),gsil(.2),gtone(1,330,.1),gsil(.5)]),[(0,2.2)],["句中停顿"])
    ps=[]
    for i in range(3):ps+=[gtone(1,220+i*100,.1),gsil(.4)]
    add("multi_sentence",np.concatenate(ps),[(0,1),(1.4,2.4),(2.8,3.8)],["连续多句"])
    add("pure_noise",gnoise(3,.03),[],["纯噪声"])
    add("pure_silence",gsil(3),[],["纯静默"])
    # Real wavs
    TW=[]
    st=os.path.join(MD,"sherpa-paraformer-online","sherpa-onnx-streaming-paraformer-bilingual-zh-en","test_wavs")
    for i in range(4):
        p=os.path.join(st,f"{i}.wav")
        if os.path.exists(p):TW.append(p)
    p=os.path.join(MD,"asr_example.wav")
    if os.path.exists(p):TW.append(p)
    for wp in TW:
        try:
            a=lwav(wp);d=len(a)/SR
            add(f"real_{os.path.basename(wp)}",a,[(0,min(d,5))],["真实音频"])
        except:pass
    return C

# ─── EnergyVad (port of Blink's vad.rs) ────────────────────────────────────

class EV:
    """EnergyVad: RMS + silence duration."""
    def __init__(self,sr=SR,thr=EVT,ms=EVMS,mn=EVMN):
        self.t=thr;self.ms=ms;self.mn=mn;self.sr=sr;self.reset()
    def reset(self):
        self.ss=0;self.sp=False;self.sl=0;self.segs=[];self.cs=None;self.ts=0
    def _r(self,s):
        if len(s)==0:return 0.0
        return float(np.sqrt(np.mean(s.astype(np.float64)**2)))
    def process(self,s):
        ev=[];msi=int(self.ms*self.sr/1000);mni=int(self.mn*self.sr/1000);sc=160
        for i in range(0,len(s),sc):
            sub=s[i:i+sc];r=self._r(sub);n=len(sub);ai=self.ts+i
            if r<self.t:
                self.ss+=n
                if self.sp:
                    if self.ss>=msi:
                        if self.sl>=mni:
                            self.sp=False;et=ai/self.sr
                            if self.cs is not None:self.segs.append((self.cs,et))
                            ev.append(('end',et))
                        else:self.sp=False;self.sl=0;self.cs=None
            else:
                if not self.sp:self.sp=True;self.sl=0;self.cs=ai/self.sr
                self.ss=0;self.sl+=n
        self.ts+=len(s);return ev
    def finalize(self):
        if self.sp and self.cs is not None:
            et=self.ts/self.sr;self.segs.append((self.cs,et));self.sp=False
        return self.segs
    def get_segs(self):return self.segs

# ─── FSMN-VAD ONNX ────────────────────────────────────────────────────────

class FV:
    """FSMN-VAD streaming ONNX. Model: fsmn-vad-onnx-v2/model_quant.onnx"""
    def __init__(self,md):
        mp=os.path.join(md,"model_quant.onnx");vp=os.path.join(md,"am.mvn")
        so=ort.SessionOptions();so.intra_op_num_threads=1
        so.graph_optimization_level=ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.s=ort.InferenceSession(mp,sess_options=so,providers=["CPUExecutionProvider"])
        self.in_=[i.name for i in self.s.get_inputs()]
        self.out_=[o.name for o in self.s.get_outputs()]
        self.means,self.vars=self._lc(vp)
        self.fl=VFL;self.fs=VFS;self.spl=5;self.mes=800;self.lb=200;self.la=100;self.fim=10
        print(f"  FSMN-VAD in={self.in_} out={self.out_}");self.reset()
    def _lc(self,p):
        m,v_=[],[];f=open(p,"r",encoding="utf-8");ls=f.readlines();f.close()
        for i in range(len(ls)):
            it=ls[i].split()
            if not it:continue
            if it[0]=="<AddShift>" and i+1<len(ls):
                ni=ls[i+1].split()
                if ni and ni[0]=="<LearnRateCoef>":m=list(map(float,ni[3:-1]))
            elif it[0]=="<Rescale>" and i+1<len(ls):
                ni=ls[i+1].split()
                if ni and ni[0]=="<LearnRateCoef>":v_=list(map(float,ni[3:-1]))
        return np.array(m,dtype=np.float32),np.array(v_,dtype=np.float32)
    def reset(self):
        self.c=[np.zeros((1,VCD,VLO,1),dtype=np.float32) for _ in range(VCL)]
        self.ic=np.array([],dtype=np.float32);self.segs=[];self.cs=None
        self.ts=0;self.sf=0;self.pf=0;self.ins=False
    def _fb(self,w):
        w=w*32768
        if len(w)>1:w=np.append(w[0],w[1:]-0.97*w[:-1])
        # Pad to at least n_fft if shorter (for single-frame chunks)
        if len(w)<512:
            w=np.pad(w,(0,512-len(w)))
        S=np.abs(librosa.stft(w,n_fft=512,win_length=self.fl,hop_length=self.fs,window='hamming',center=True))
        mb=librosa.filters.mel(sr=SR,n_fft=512,n_mels=NM,fmin=0.0,fmax=SR/2)
        fb=np.dot(mb,S);fb=np.log(np.maximum(fb,1e-10));return fb.T.astype(np.float32)
    def _sp(self,f):
        if len(f)<self.spl:return np.zeros((0,400),dtype=np.float32)
        return np.array([f[i:i+self.spl].flatten() for i in range(len(f)-self.spl+1)],dtype=np.float32)
    def _cm(self,f):return(f+self.means)*self.vars
    def process(self,s):
        ev=[]
        if len(self.ic)>0:s=np.concatenate([self.ic,s])
        self.ic=np.array([],dtype=np.float32)
        if len(s)<self.fl:self.ic=s;return ev
        nf=(len(s)-self.fl)//self.fs+1
        if nf<1:self.ic=s;return ev
        us=(nf-1)*self.fs+self.fl;wd=s[:us];self.ic=s[us:]
        fb=self._fb(wd);sp=self._sp(fb)
        if len(sp)==0:return ev
        ft=self._cm(sp);sp_in=ft[np.newaxis,:,:]
        feeds={self.in_[0]:sp_in}
        for i in range(VCL):feeds[self.in_[1+i]]=self.c[i]
        o=self.s.run(self.out_,feeds);lg=o[0]
        for i in range(VCL):self.c[i]=o[1+i]
        lp=lg[0];pr=np.exp(lp-np.max(lp,axis=-1,keepdims=True));pr=pr/np.sum(pr,axis=-1,keepdims=True)
        spp=1.0-pr[:,0];dec=(spp>0.5).astype(int)
        sm=dec.copy()
        for i in range(1,len(dec)-1):sm[i]=1 if np.sum(dec[i-1:i+2])>=2 else 0
        fss=self.fs/SR;cs=self.ts/SR
        for fi,isp in enumerate(sm):
            ft_s=cs+fi*fss
            if isp:
                self.sf=0;self.pf+=1
                if not self.ins:
                    self.ins=True;self.cs=max(0,ft_s-self.lb/1000)
            else:
                self.pf=0;self.sf+=1
                if self.ins:
                    sm_ms=self.sf*self.fim
                    if sm_ms>=self.mes:
                        et=ft_s+self.la/1000
                        if self.cs is not None:self.segs.append((self.cs,et))
                        ev.append(('end',et))
                        self.ins=False;self.cs=None;self.sf=0
        self.ts+=us;return ev
    def finalize(self):
        if self.ins and self.cs is not None:
            et=self.ts/SR;self.segs.append((self.cs,et));self.ins=False
        return self.segs
    def get_segs(self):return self.segs

# ─── ParaformerOnline (from C2) ───────────────────────────────────────────

def lc_asr(p):
    m,v_=[],[];f=open(p,"r",encoding="utf-8");ls=f.readlines();f.close()
    for i in range(len(ls)):
        li=ls[i].split()
        if len(li)>0 and li[0]=="<AddShift>" and i+1<len(ls):
            ni=ls[i+1].split()
            if len(ni)>0 and ni[0]=="<LearnRateCoef>":m=list(map(float,ni[3:-1]))
        elif len(li)>0 and li[0]=="<Rescale>" and i+1<len(ls):
            ni=ls[i+1].split()
            if len(ni)>0 and ni[0]=="<LearnRateCoef>":v_=list(map(float,ni[3:-1]))
    m2=np.array(m,dtype=np.float32);v2=np.array(v_,dtype=np.float32)
    fd2=LM*NM
    if len(m2)<fd2:m2=np.tile(m2,(10,))[:fd2];v2=np.tile(v2,(10,))[:fd2]
    return m2,v2

def fb_kaldi(w,sr=SR):
    w=np.append(w[0],w[1:]-0.97*w[:-1])
    S=np.abs(librosa.stft(w,n_fft=512,win_length=int(sr*FL_MS/1000),hop_length=int(sr*FS_MS/1000),window='hamming',center=False))
    mb=librosa.filters.mel(sr=sr,n_fft=512,n_mels=NM,fmin=0.0,fmax=sr/2)
    fb=np.dot(mb,S);fb=np.log(np.maximum(fb,1e-10));return fb.T.astype(np.float32)

def ld_tok(p):
    f=open(p,"r",encoding="utf-8");td=json.load(f);f.close()
    if isinstance(td,dict):
        mx=max(int(k) for k in td.keys())
        return[td[str(i)] if str(i) in td else "" for i in range(mx+1)]
    elif isinstance(td,list):return td
    return[]

class PO:
    """ParaformerOnline streaming oracle (from Spike C2)."""
    def __init__(self,md):
        ep=os.path.join(md,"encoder.onnx");dp=os.path.join(md,"decoder.onnx")
        mp=os.path.join(md,"am.mvn");tp=os.path.join(md,"tokens.json")
        self.means,self.vars=lc_asr(mp);self.tokens=ld_tok(tp)
        so=ort.SessionOptions();so.intra_op_num_threads=1
        so.graph_optimization_level=ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.enc=ort.InferenceSession(ep,sess_options=so,providers=["CPUExecutionProvider"])
        self.dec=ort.InferenceSession(dp,sess_options=so,providers=["CPUExecutionProvider"])
        self.reset()
    def reset(self):
        self.si=0;self.i1=True;self.il=False;self.ic=[];self.lc=[]
        self.hc=np.zeros((1,ES),dtype=np.float32);self.ac=np.array([0.0],dtype=np.float32)
        self.fc=np.zeros((CZ[0]+CZ[2],FD),dtype=np.float32)
        self.dc=[np.zeros((1,FD2,FR),dtype=np.float32) for _ in range(FL)]
    def _lfr(self,wf,dn):
        m,n=LM,LN
        if self.lc:wf=np.vstack([np.array(self.lc,dtype=np.float32),wf])
        T=len(wf);Tl=int(np.ceil((T-(m-1)//2)/float(n)));out=[];stop=Tl
        for i in range(Tl):
            if m<=T-i*n:
                p=[]
                for j in range(m):p.extend(wf[i*n+j])
                out.append(p)
            elif dn:
                np_=m-(T-i*n);p=[]
                for j in range(T-i*n):p.extend(wf[i*n+j])
                for _ in range(np_):p.extend(wf[-1])
                out.append(p)
            else:stop=i;break
        lsi=min(stop*n,T-1);self.lc=wf[lsi:].tolist() if lsi<T else[]
        if out:
            out=np.array(out,dtype=np.float32);out=(out+self.means)*self.vars
        else:out=np.zeros((0,FD),dtype=np.float32)
        return out
    def _pe(self,wf):
        ts=wf.shape[0];fd=wf.shape[1];si=self.si;self.si+=ts;sc=-0.0330119726594128
        pe=np.zeros((self.si,fd),dtype=np.float32)
        for i in range(fd//2):
            tm=math.exp(i*sc)
            for j in range(self.si):
                coe=tm*(j+1);pe[j,i]=math.sin(coe);pe[j,i+fd//2]=math.cos(coe)
        wf+=pe[si:si+ts];return wf
    def _ol(self,wf,dn):
        if len(self.fc)>0:wf=np.vstack([self.fc,wf])
        if dn:
            nc=wf[-CZ[0]:] if len(wf)>=CZ[0] else wf
            if not self.il:
                pl=sum(CZ)-len(wf)
                if 0<pl<100:wf=np.vstack([wf,np.zeros((pl,FD),dtype=np.float32)])
        else:
            cl=CZ[0]+CZ[2];nc=wf[-cl:] if len(wf)>=cl else wf
        self.fc=nc;return wf
    def _cif(self,h,a,il):
        if len(h)==0:return np.zeros((0,ES),dtype=np.float32)
        hs=h.shape[1];a[:CZ[0]]=0.0;a[sum(CZ[:-1]):]=0.0
        if len(self.hc)>0:h=np.vstack([self.hc,h]);a=np.concatenate([self.ac,a])
        if il:h=np.vstack([h,np.zeros((1,hs),dtype=np.float32)]);a=np.append(a,TA)
        lf=[];ig=0.0;fr=np.zeros(hs,dtype=np.float32)
        for i in range(len(a)):
            al=a[i]
            if al+ig<CT:ig+=al;fr+=al*h[i]
            else:
                fr+=(CT-ig)*h[i];lf.append(fr.copy());ig+=al;ig-=CT;fr=ig*h[i]
        self.ac=np.array([ig],dtype=np.float32)
        self.hc=(fr/ig).reshape(1,-1) if ig>0 else fr.reshape(1,-1)
        return np.array(lf,dtype=np.float32) if lf else np.zeros((0,hs),dtype=np.float32)
    def _gd(self,lg):
        r=[]
        for row in lg:
            idx=np.argmax(row)
            if idx<len(self.tokens):
                t=self.tokens[idx]
                if t in("<eos>","</s>"):break
                r.append(t)
        return"".join(r)
    def fchunk(self,cf,dn):
        t0=time.time();res=""
        if len(cf)==0:return res,0.0,0
        cf=cf*math.sqrt(ES);cf=self._pe(cf);cf=self._ol(cf,dn);nf=len(cf)
        ei0=self.enc.get_inputs()[0].name;ei1=self.enc.get_inputs()[1].name
        sp=cf[np.newaxis,:,:].astype(np.float32);sl=np.array([nf],dtype=np.int32)
        eo=self.enc.run(None,{ei0:sp,ei1:sl});enc=eo[0][0];alp=eo[2][0]
        lf=self._cif(enc,alp,self.il);lfc=len(lf)
        if lfc>0:
            dn_=[i.name for i in self.dec.get_inputs()]
            di={dn_[0]:eo[0],dn_[1]:eo[1],dn_[2]:lf[np.newaxis,:,:],dn_[3]:np.array([lfc],dtype=np.int32)}
            for l in range(FL):di[dn_[4+l]]=self.dc[l]
            do=self.dec.run(None,di);res=self._gd(do[0][0])
            for l in range(FL):self.dc[l]=do[2+l]
        return res,(time.time()-t0)*1000,nf
    def fwd(self,ca,dn):
        dur=len(ca)/SR
        if len(ca)<960 and dn and not self.i1:
            self.il=True;wf=self.fc.copy();r,ms,nf=self.fchunk(wf,self.il);self.reset()
            return r,{"inf_ms":ms,"nf":nf,"dur":dur,"pt":"","if_":True}
        self.i1=False;waves=list(self.ic)+list(ca)
        fsl=SR*FL_MS//1000;fss=SR*FS_MS//1000
        fn=(len(waves)-fsl)//fss+1 if len(waves)>=fsl else 0
        if fn<1 or len(waves)<fsl:
            self.ic=waves;return"",{"inf_ms":0,"nf":0,"dur":dur,"pt":"","if_":dn}
        self.ic=waves[fn*fss:];tl=min(fn*fss-fss+fsl,len(waves))
        s=np.array(waves[:tl],dtype=np.float32);wf=fb_kaldi(s*32768)
        if wf.shape[0]==0:
            if dn:self.ic=[];self.lc=[]
            return"",{"inf_ms":0,"nf":0,"dur":dur,"pt":"","if_":dn}
        if not self.lc:
            ff=wf[0].tolist()
            for _ in range((LM-1)//2):self.lc.append(ff)
        total=wf.shape[0]+len(self.lc)
        if total>=LM:
            lfr=self._lfr(wf,dn)
            if lfr.shape[0]==0:
                if dn:self.ic=[];self.lc=[]
                return"",{"inf_ms":0,"nf":0,"dur":dur,"pt":"","if_":dn}
            r,ms,nf=self.fchunk(lfr,dn)
            if dn:self.reset()
            return r,{"inf_ms":ms,"nf":nf,"dur":dur,"pt":r,"if_":dn}
        else:
            for i in range(wf.shape[0]):self.lc.append(wf[i].tolist())
            return"",{"inf_ms":0,"nf":0,"dur":dur,"pt":"","if_":dn}

# ─── Paraformer Offline ───────────────────────────────────────────────────

class PF:
    """Paraformer offline ASR (整段推理)."""
    def __init__(self,md):
        mp=os.path.join(md,"model_quant.onnx");vp=os.path.join(md,"am.mvn");tp=os.path.join(md,"tokens.json")
        self.means,self.vars=lc_asr(vp);self.tokens=ld_tok(tp)
        so=ort.SessionOptions();so.intra_op_num_threads=1
        so.graph_optimization_level=ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.s=ort.InferenceSession(mp,sess_options=so,providers=["CPUExecutionProvider"])
        print(f"  PF offline in={[(i.name,i.shape) for i in self.s.get_inputs()]}")
    def transcribe(self,audio):
        t0=time.time();wav=audio*32768;wf=fb_kaldi(wav.astype(np.float32))
        m,n=LM,LN;T=len(wf)
        if T<m:return"",0.0
        Tl=int(np.ceil((T-(m-1)//2)/float(n)));out=[]
        for i in range(Tl):
            if m<=T-i*n:
                p=[]
                for j in range(m):p.extend(wf[i*n+j])
                out.append(p)
            else:
                np_=m-(T-i*n);p=[]
                for j in range(T-i*n):p.extend(wf[i*n+j])
                for _ in range(np_):p.extend(wf[-1])
                out.append(p)
        ft=np.array(out,dtype=np.float32);ft=(ft+self.means)*self.vars
        i0=self.s.get_inputs()[0].name;i1=self.s.get_inputs()[1].name
        sp=ft[np.newaxis,:,:].astype(np.float32);sl=np.array([len(ft)],dtype=np.int32)
        o=self.s.run(None,{i0:sp,i1:sl});lg=o[0][0]
        r=[]
        for row in lg:
            idx=np.argmax(row)
            if idx<len(self.tokens):
                t=self.tokens[idx]
                if t in("<eos>","</s>"):break
                r.append(t)
        return"".join(r),(time.time()-t0)*1000

# ─── VAD Evaluation ───────────────────────────────────────────────────────

def _iou(s1,s2):
    s=max(s1[0],s2[0]);e=min(s1[1],s2[1]);iv=max(0,e-s)
    un=(s1[1]-s1[0])+(s2[1]-s2[0])-iv;return iv/un if un>0 else 0.0

def eval_vad(det,gt,dur,thr=0.3):
    if not gt and not det:
        return{"onset_s":0,"endpoint_s":0,"fa":0,"fr":0,"p":1,"r":1,"f1":1,"over":0,"under":0,"miss":0,"correct":0,"n_gt":0,"n_det":0}
    mg=set();md=set();od=[];ed=[]
    for di,ds_ in enumerate(det):
        for gi,gs_ in enumerate(gt):
            if gi in mg:continue
            if _iou(ds_,gs_)>=thr:
                mg.add(gi);md.add(di);od.append(abs(ds_[0]-gs_[0]));ed.append(abs(ds_[1]-gs_[1]));break
    fa=len(det)-len(md);fr=len(gt)-len(mg)
    over=0
    for gi in range(len(gt)):
        if gi not in mg:continue
        cnt=sum(1 for di in range(len(det)) if di not in md and _iou(det[di],gt[gi])>0)
        if cnt>0:over+=cnt
    p=len(md)/len(det) if det else 0
    r=len(mg)/len(gt) if gt else 0
    f1=2*p*r/(p+r) if(p+r)>0 else 0
    return{
        "onset_s":round(float(np.mean(od)),3) if od else None,
        "endpoint_s":round(float(np.mean(ed)),3) if ed else None,
        "fa":fa,"fr":fr,"p":round(p,3),"r":round(r,3),"f1":round(f1,3),
        "over":over,"under":fr,"miss":fr,"correct":len(mg),
        "n_gt":len(gt),"n_det":len(det),
    }

def cer(ref,hyp):
    if not ref:return 0.0 if not hyp else 1.0
    if not hyp:return 1.0
    r=list(ref);h=list(hyp);m=len(r);n=len(h)
    d=[[0]*(n+1) for _ in range(m+1)]
    for i in range(m+1):d[i][0]=i
    for j in range(n+1):d[0][j]=j
    for i in range(1,m+1):
        for j in range(1,n+1):
            if r[i-1]==h[j-1]:d[i][j]=d[i-1][j-1]
            else:d[i][j]=1+min(d[i-1][j],d[i][j-1],d[i-1][j-1])
    return d[m][n]/m

# ─── Combo Runner ─────────────────────────────────────────────────────────

def run_combo(combo_id,vad_type,asr_type,corpus,vad_inst,asr_inst):
    results=[]
    for item in corpus:
        audio=item["audio"];gt=item["segments"];dur=len(audio)/SR
        if hasattr(vad_inst,'reset'):vad_inst.reset()
        chunk_size=1600;vad_segs=[];first_endpoint=None
        vad_t0=time.time()
        for i in range(0,len(audio),chunk_size):
            chunk=audio[i:i+chunk_size]
            ev=vad_inst.process(chunk) if hasattr(vad_inst,'process') else vad_inst.process_chunk(chunk)
            for et,en in ev:
                if first_endpoint is None:first_endpoint=en
        if hasattr(vad_inst,'finalize'):vad_inst.finalize()
        vad_segs=vad_inst.get_segs()
        vad_time=time.time()-vad_t0
        ve=eval_vad(vad_segs,gt,dur)
        # ASR
        asr_text="";asr_rtf=0;asr_fp=None;asr_ff=None;asr_note=""
        if asr_type=="paraformer_online" and asr_inst is not None:
            asr_inst.reset();all_text=[];first_partial=None;t0=time.time()
            n_chunks=max(1,(len(audio)-1)//CSS+1)
            for ci in range(n_chunks):
                s=ci*CSS;e=min((ci+1)*CSS,len(audio));ca=audio[s:e];is_f=ci==n_chunks-1
                txt,info=asr_inst.fwd(ca,is_f)
                if txt:
                    if first_partial is None:first_partial=time.time()-t0
                    all_text.append(txt)
            asr_time=time.time()-t0
            asr_text="".join(all_text);asr_rtf=asr_time/dur if dur>0 else 0
            asr_fp=first_partial;asr_ff=asr_time if all_text else None
        elif asr_type=="paraformer_offline" and asr_inst is not None:
            t0=time.time();txt,ms=asr_inst.transcribe(audio);tt=time.time()-t0
            asr_text=txt;asr_rtf=tt/dur if dur>0 else 0;asr_ff=tt
        elif asr_type in("sensevoice_onnx",):
            asr_text="NOT_AVAILABLE";asr_note="Model not downloaded"
        elif asr_type=="gguf_nano":
            asr_text="NOT_MEASURED";asr_note="C++ worker, not tested in Python"
        mem_mb=psutil.Process().memory_info().rss/(1024*1024) if psutil else 0
        results.append({
            "item":item["name"],"conditions":item["conditions"],
            "audio_dur_s":round(dur,3),"gt_segments":gt,"det_segments":vad_segs,
            "vad_eval":ve,"vad_time_s":round(vad_time,3),
            "asr_text":asr_text,"asr_rtf":round(asr_rtf,4),
            "asr_first_partial_s":round(asr_fp,3) if asr_fp else None,
            "asr_first_final_s":round(asr_ff,3) if asr_ff else None,
            "mem_mb":round(mem_mb,1),"asr_note":asr_note,
        })
    return results
