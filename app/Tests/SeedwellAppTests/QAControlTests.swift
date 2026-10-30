import Testing
@testable import SeedwellApp

/// The QA control file's protocol (QAControl): the lines a script appended, and `wait`'s argument.
@MainActor @Suite struct QAControlTests {
    @Test func onlyCompleteLinesAreRunAndEmptyOnesCount() {
        // A line still being written (no newline yet) waits; an empty line counts, as `wc -l` counts it in qa_cmd.
        #expect(QAControl.completeLines("") == [])
        #expect(QAControl.completeLines("section sharing") == [])
        #expect(QAControl.completeLines("section sharing\n") == ["section sharing"])
        #expect(QAControl.completeLines("a\n\nb\nwind") == ["a", "", "b"])
    }

    @Test func waitTakesACommandATextAndATimeout() {
        #expect(QAControl.WaitSpec("where ~ section downloads") == .init(command: "where", text: "section downloads", absent: false, limit: 15))
        #expect(QAControl.WaitSpec("starting state ~ shown true timeout 4.5")
            == .init(command: "starting state", text: "shown true", absent: false, limit: 4.5))
        #expect(QAControl.WaitSpec("windows !~ chooser timeout 3") == .init(command: "windows", text: "chooser", absent: true, limit: 3))
        // The text may hold spaces and a tilde; "timeout" only counts at the end with a number.
        #expect(QAControl.WaitSpec("menu-titles ~ a ~b timeout x")?.text == "a ~b timeout x")
        #expect(QAControl.WaitSpec("ui state ~ rows [\"Night Harbor")?.text == "rows [\"Night Harbor")
        #expect(QAControl.WaitSpec("where ~ timeout window") == .init(command: "where", text: "timeout window", absent: false, limit: 15))
    }

    @Test func waitRefusesWhatItCannotRun() {
        #expect(QAControl.WaitSpec("where") == nil)
        #expect(QAControl.WaitSpec(" ~ text") == nil)
        #expect(QAControl.WaitSpec("where ~ ") == nil)
        #expect(QAControl.WaitSpec("wait where ~ x ~ y") == nil)
    }
}
