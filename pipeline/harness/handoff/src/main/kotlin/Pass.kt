import com.fasterxml.jackson.core.JsonParser
import com.fasterxml.jackson.databind.DeserializationFeature
import com.fasterxml.jackson.databind.ObjectMapper
import java.io.File
import kotlin.math.ceil

private val mapper = ObjectMapper()
    .enable(JsonParser.Feature.STRICT_DUPLICATE_DETECTION)
    .enable(DeserializationFeature.FAIL_ON_TRAILING_TOKENS)

private fun checkedBytes(input: ByteArray, request: String, graph: String): ByteArray {
    val json = mapper.readTree(input)
    val version = json["schema_version"]
    require(version?.isIntegralNumber == true && version.canConvertToInt() && version.intValue() == 1)
    require(json["plan_request_id"]?.textValue() == request)
    require(json["graph_version"]?.textValue() == graph)
    return input
}

fun main(args: Array<String>) {
    val (source, request, graph, scorer) = args
    val raw = File(source).readBytes()
    val repeats = args.getOrNull(4)?.toInt() ?: 1
    require(repeats in 1..1000)
    val samples = mutableListOf<Long>()
    val worker = ProcessBuilder(scorer).redirectError(ProcessBuilder.Redirect.INHERIT).start()
    val outputs = worker.inputStream.bufferedReader()
    try {
        worker.outputStream.buffered().use { pipe ->
            repeat(repeats) {
                val start = System.nanoTime()
                val payload = checkedBytes(raw, request, graph)
                check(payload === raw)
                pipe.write(payload)
                pipe.write(10) // ホスト試験専用の NDJSON フレーム境界。
                pipe.flush()
                val result = requireNotNull(outputs.readLine()) { "Rust が入力を拒否" }
                samples += System.nanoTime() - start
                if (it == 0) println(result)
            }
        }
        check(worker.waitFor() == 0)
    } finally {
        outputs.close()
        worker.destroy()
    }
    val p95 = samples.sorted()[ceil(.95 * repeats).toInt()-1] / 1_000_000.0
    System.err.println("host_pipe_p95_ms=$p95 repeats=$repeats investigate=${p95 > 300.0}")
}
