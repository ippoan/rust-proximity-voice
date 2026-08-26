// マイクの常時リングバッファ (docs/protocol.md §4)。
//
// PTT は「V を押した」ことをゲームサーバー → リレー → ブラウザと回ってから届くので、
// { "t":"talk", "on":true } を受け取った時点で最初の音節は既に過ぎている。
// そこで **喋っていない間もマイクをリングに溜め続け**、talk on で遡って読み出す。
//
// 送信していない間の出力は完全な無音。加えて main 側が sender.replaceTrack(null) で
// トラック自体を外すので、PTT を押していない間は 1 バイトも出て行かない (二重の歯止め)。
class MicRingProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = (options && options.processorOptions) || {};
    const bufferMs = o.bufferMs || 300;
    this.capacity = Math.max(1024, Math.ceil(sampleRate * bufferMs / 1000));
    this.buf = new Float32Array(this.capacity);
    this.written = 0;   // 総書き込みサンプル数 (絶対位置)
    this.read = 0;      // 総読み出しサンプル数 (絶対位置)。送信中だけ進む
    this.sending = false;
    this.draining = false;
    // 遡り量。サーバー往復 (50〜100ms) を吸えるだけ遡る。バッファ長より必ず短くする
    this.lookback = Math.min(
      Math.round(sampleRate * (o.lookbackMs || 150) / 1000),
      this.capacity - 2048
    );
    // 無音とみなす振幅。ここを飛ばして遅延を詰める (下の「追いつき」)
    this.silence = o.silencePeak || 0.008;

    this.port.onmessage = (e) => {
      const m = e.data || {};
      if (m.t === 'start') {
        const back = Math.min(this.lookback, this.written);
        this.read = this.written - back;
        this.sending = true;
        this.draining = false;
        this.port.postMessage({ t: 'started', lookbackMs: (back / sampleRate) * 1000 });
      } else if (m.t === 'stop') {
        // ここで即座に止めると、遡ったぶんだけ末尾が落ちる。溜まりを吐き切ってから止める
        if (!this.sending) { this.port.postMessage({ t: 'drained' }); return; }
        this.draining = true;
      }
    };
  }

  _peak(from, n) {
    let p = 0;
    for (let i = 0; i < n; i++) {
      const v = Math.abs(this.buf[(from + i) % this.capacity]);
      if (v > p) p = v;
    }
    return p;
  }

  process(inputs, outputs) {
    const inCh = inputs[0] && inputs[0][0];
    const outCh = outputs[0] && outputs[0][0];
    const n = outCh ? outCh.length : 128;

    // 1. 常に書く。喋っていなくても溜め続けるのがこの worklet の存在理由
    if (inCh) {
      for (let i = 0; i < inCh.length; i++) this.buf[(this.written + i) % this.capacity] = inCh[i];
      this.written += inCh.length;
    } else {
      for (let i = 0; i < n; i++) this.buf[(this.written + i) % this.capacity] = 0;
      this.written += n;
    }

    if (!outCh) return true;
    if (!this.sending) { outCh.fill(0); return true; }

    // 2. 追いつき: 遡ったぶんの遅延を、無音区間を捨てて詰める。
    //    音の出ている区間は絶対に捨てない (話者が早口になるより、少し遅れるほうがよい)
    if (this.written - this.read > n && this._peak(this.read, n) < this.silence) {
      this.read += n;
    }

    // 3. 読み出す
    for (let i = 0; i < n; i++) {
      const at = this.read + i;
      outCh[i] = at < this.written ? this.buf[at % this.capacity] : 0;
    }
    this.read += n;

    if (this.draining && this.read >= this.written) {
      this.sending = false;
      this.draining = false;
      this.port.postMessage({ t: 'drained' });
    }
    return true;
  }
}

registerProcessor('mic-ring', MicRingProcessor);
