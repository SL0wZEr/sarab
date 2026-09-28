/*
 * The decode probe: whether Android's MediaCodec decoders work under Sarab,
 * without an app. Each file given is demuxed with MediaExtractor and its first
 * track decoded to ByteBuffers (or, after `--surface`, to a detached
 * SurfaceTexture, the way a video player hands frames on) for at most five
 * seconds; `main` prints the codec, and how many output buffers and bytes came
 * out, or the exception. `--parallel N R FILES` runs N decodes at once, R
 * rounds, and prints the count that decoded and the failures by kind: the
 * test for allocators that break under concurrent use.
 *
 * The findings it produced are in docs/TODO.md ("No media decoding"). Any
 * change that could reach the GPU (a gralloc or Codec2 pool property) is run
 * first on a throwaway data directory with the kernel log watched: the BLOB
 * pool under `--parallel` reset the host GPU.
 *
 * Built with the SDK's javac and d8 against android-33, run by run.sh through
 * `app_process`; run.sh's header has the commands.
 */
import android.media.MediaCodec;
import android.media.MediaExtractor;
import android.media.MediaFormat;
import android.graphics.SurfaceTexture;
import android.view.Surface;
import java.nio.ByteBuffer;

public class Probe {
    public static void main(String[] args) throws Exception {
        if (args.length > 2 && args[0].equals("--parallel")) {
            parallel(Integer.parseInt(args[1]), Integer.parseInt(args[2]), java.util.Arrays.copyOfRange(args, 3, args.length));
            System.exit(0);
        }
        boolean surface = args.length > 0 && args[0].equals("--surface");
        for (String path : args) {
            if (path.equals("--surface")) continue;
            SURFACE = surface;
            try {
                System.out.println(path + ": " + decode(path));
            } catch (Throwable t) {
                System.out.println(path + ": FAILED " + t);
            }
        }
        System.exit(0);
    }

    static boolean SURFACE;

    static void parallel(int threads, int rounds, String[] files) throws Exception {
        final java.util.concurrent.atomic.AtomicInteger ok = new java.util.concurrent.atomic.AtomicInteger();
        final java.util.concurrent.ConcurrentHashMap<String, Integer> bad = new java.util.concurrent.ConcurrentHashMap<>();
        for (int r = 0; r < rounds; r++) {
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final String path = files[t % files.length];
                ts[t] = new Thread(() -> {
                    try {
                        String res = decode(path);
                        if (res.contains(" 0 output buffers")) bad.merge(path + ": no output", 1, Integer::sum);
                        else ok.incrementAndGet();
                    } catch (Throwable e) {
                        bad.merge(path + ": " + e, 1, Integer::sum);
                    }
                });
            }
            for (Thread t : ts) t.start();
            for (Thread t : ts) t.join();
        }
        System.out.println("parallel " + threads + "x" + rounds + ": " + ok.get() + " decoded, failures " + bad);
    }

    static String decode(String path) throws Exception {
        MediaExtractor ex = new MediaExtractor();
        ex.setDataSource(path);
        MediaFormat f = ex.getTrackFormat(0);
        String mime = f.getString(MediaFormat.KEY_MIME);
        ex.selectTrack(0);
        MediaCodec c = MediaCodec.createDecoderByType(mime);
        String name = c.getName();
        Surface out = null;
        if (SURFACE) {
            SurfaceTexture st = new SurfaceTexture(false);
            st.setDefaultBufferSize(f.getInteger(MediaFormat.KEY_WIDTH), f.getInteger(MediaFormat.KEY_HEIGHT));
            out = new Surface(st);
        }
        c.configure(f, out, null, 0);
        c.start();
        MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
        int frames = 0;
        long bytes = 0;
        boolean inEos = false;
        long deadline = System.currentTimeMillis() + 5000;
        while (System.currentTimeMillis() < deadline) {
            if (!inEos) {
                int i = c.dequeueInputBuffer(10000);
                if (i >= 0) {
                    ByteBuffer b = c.getInputBuffer(i);
                    int n = ex.readSampleData(b, 0);
                    if (n < 0) {
                        c.queueInputBuffer(i, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM);
                        inEos = true;
                    } else {
                        c.queueInputBuffer(i, 0, n, ex.getSampleTime(), 0);
                        ex.advance();
                    }
                }
            }
            int o = c.dequeueOutputBuffer(info, 10000);
            if (o >= 0) {
                if (info.size > 0) { frames++; bytes += info.size; }
                c.releaseOutputBuffer(o, SURFACE);
                if ((info.flags & MediaCodec.BUFFER_FLAG_END_OF_STREAM) != 0) break;
            }
        }
        c.stop();
        c.release();
        return name + " (" + mime + "): " + frames + " output buffers, " + bytes + " bytes";
    }
}
