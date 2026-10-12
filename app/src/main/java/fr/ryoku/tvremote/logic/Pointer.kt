package fr.ryoku.tvremote.logic

/**
 * Turns touchpad drags (fractional pixels) into the whole-pixel moves the TV takes,
 * keeping the remainder so slow drags still move. A faster drag moves further
 * (simple acceleration). Thread-safe.
 */
class PointerAccumulator(
    private val gain: Float = 1.2f,
    private val accel: Float = 0.04f,
    private val maxGain: Float = 4f,
) {
    private var x = 0f
    private var y = 0f

    @Synchronized
    fun add(dx: Float, dy: Float) {
        val speed = kotlin.math.sqrt(dx * dx + dy * dy)
        val g = (gain + accel * speed).coerceAtMost(maxGain)
        x += dx * g
        y += dy * g
    }

    /** The whole pixels to send now, or null if less than one pixel accumulated. */
    @Synchronized
    fun take(): Pair<Int, Int>? {
        val ix = x.toInt()
        val iy = y.toInt()
        if (ix == 0 && iy == 0) return null
        x -= ix
        y -= iy
        return ix to iy
    }

    @Synchronized
    fun reset() {
        x = 0f
        y = 0f
    }
}

/** Broadcast address of an IPv4 address (4 bytes) with a prefix length, as text. */
fun ipv4Broadcast(address: ByteArray, prefix: Int): String? {
    if (address.size != 4 || prefix !in 0..32) return null
    val ip = address.fold(0L) { acc, b -> (acc shl 8) or (b.toLong() and 0xFF) }
    val mask = if (prefix == 0) 0L else (0xFFFFFFFFL shl (32 - prefix)) and 0xFFFFFFFFL
    val bc = (ip and mask) or (mask.inv() and 0xFFFFFFFFL)
    return (3 downTo 0).joinToString(".") { ((bc shr (it * 8)) and 0xFF).toString() }
}
