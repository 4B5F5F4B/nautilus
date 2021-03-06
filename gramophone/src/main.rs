#![feature(vec_remove_item)]
extern crate antlr_parser;
extern crate forksrv;
extern crate grammartec;
extern crate serde_json;
extern crate time as othertime;
#[macro_use]
extern crate serde_derive;
extern crate clap;
extern crate ron;

mod config;
mod fuzzer;
mod queue;
mod rules;
mod shared_state;
mod state;

use config::Config;
use forksrv::error::SubprocessError;
use fuzzer::Fuzzer;
use grammartec::chunkstore::ChunkStoreWrapper;
use grammartec::context::{Context, SerializableContext};
use queue::{InputState, QueueItem};
use shared_state::GlobalSharedState;
use state::FuzzingState;

use clap::{App, Arg};
use othertime::strftime;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{thread, time};

fn process_input(
    state: &mut FuzzingState,
    inp: &mut QueueItem,
    config: &Config,
) -> Result<(), SubprocessError> {
    match inp.state {
        InputState::Init(start_index) => {
            let end_index = start_index + 200;

            if state.minimize(inp, start_index, end_index)? {
                inp.state = InputState::Det((0, 0));
            } else {
                inp.state = InputState::Init(end_index);
            }
        }
        InputState::Det((cycle, start_index)) => {
            let end_index = start_index + 1;
            if state.deterministic_tree_mutation(inp, start_index, end_index)? {
                if cycle == config.number_of_deterministic_mutations {
                    inp.state = InputState::DetAFL(0);
                } else {
                    inp.state = InputState::Det((cycle + 1, 0));
                }
            } else {
                inp.state = InputState::Det((cycle, end_index));
            }
            state.splice(inp)?;
            state.havoc(inp)?;
            state.havoc_recursion(inp)?;
        }
        InputState::DetAFL(start_index) => {
            let end_index = start_index + 1;
            if state.deterministic_afl_mutation(inp, start_index, end_index)? {
                inp.state = InputState::Random;
            } else {
                inp.state = InputState::DetAFL(end_index);
            }
            state.splice(inp)?;
            state.havoc(inp)?;
            state.havoc_recursion(inp)?;
        }
        InputState::Random => {
            state.splice(inp)?;
            state.havoc(inp)?;
            state.havoc_recursion(inp)?;
        }
    }
    return Ok(());
}

fn fuzzing_thread(
    global_state: Arc<Mutex<GlobalSharedState>>,
    config: Config,
    ctx: Context,
    cks: Arc<ChunkStoreWrapper>,
) {
    let path_to_bin_target = config.path_to_bin_target.to_owned();
    let args = config.arguments.clone();

    let fuzzer = Fuzzer::new(
        path_to_bin_target.clone(),
        args,
        global_state.clone(),
        config.dump_mode,
        config.path_to_workdir.clone(),
    ).expect("RAND_3617502350");
    let mut state = FuzzingState::new(fuzzer, config.clone(), cks.clone());
    state.ctx = ctx.clone();
    let mut old_execution_count = 0;
    let mut old_executions_per_sec = 0;
    //Normal mode
    if config.no_feedback_mode == false {
        loop {
            let inp = global_state.lock().expect("RAND_2191486322").queue.pop();
            if let Some(mut inp) = inp {
                //If subprocess died restart forkserver
                if process_input(&mut state, &mut inp, &config).is_err() {
                    let args = vec![];
                    let fuzzer = Fuzzer::new(
                        path_to_bin_target.clone(),
                        args,
                        global_state.clone(),
                        config.dump_mode,
                        config.path_to_workdir.clone(),
                    ).expect("RAND_3077320530");
                    state = FuzzingState::new(fuzzer, config.clone(), cks.clone());
                    state.ctx = ctx.clone();
                    old_execution_count = 0;
                    old_executions_per_sec = 0;
                }
                global_state
                    .lock()
                    .expect("RAND_788470278")
                    .queue
                    .finished(inp);
            } else {
                for _ in 0..config.number_of_generate_inputs {
                    //If subprocess dies restart forkserver
                    if state.generate_random("START").is_err() {
                        let args = vec![];
                        let fuzzer = Fuzzer::new(
                            path_to_bin_target.clone(),
                            args,
                            global_state.clone(),
                            config.dump_mode,
                            config.path_to_workdir.clone(),
                        ).expect("RAND_357619639");
                        state = FuzzingState::new(fuzzer, config.clone(), cks.clone());
                        state.ctx = ctx.clone();
                        old_execution_count = 0;
                        old_executions_per_sec = 0;
                    }
                }
                global_state
                    .lock()
                    .expect("RAND_2035137253")
                    .queue
                    .new_round();
            }
            let mut stats = global_state.lock().expect("RAND_2403514078");
            stats.execution_count += state.fuzzer.execution_count - old_execution_count;
            old_execution_count = state.fuzzer.execution_count;
            stats.average_executions_per_sec += state.fuzzer.average_executions_per_sec as u32;
            stats.average_executions_per_sec -= old_executions_per_sec;
            old_executions_per_sec = state.fuzzer.average_executions_per_sec as u32;
            if state.fuzzer.bits_found_by_havoc > 0 {
                stats.bits_found_by_havoc += state.fuzzer.bits_found_by_havoc;
                state.fuzzer.bits_found_by_havoc = 0;
            }
            if state.fuzzer.bits_found_by_gen > 0 {
                stats.bits_found_by_gen += state.fuzzer.bits_found_by_gen;
                state.fuzzer.bits_found_by_gen = 0;
            }
            if state.fuzzer.bits_found_by_min > 0 {
                stats.bits_found_by_min += state.fuzzer.bits_found_by_min;
                state.fuzzer.bits_found_by_min = 0;
            }
            if state.fuzzer.bits_found_by_det > 0 {
                stats.bits_found_by_det += state.fuzzer.bits_found_by_det;
                state.fuzzer.bits_found_by_det = 0;
            }
            if state.fuzzer.bits_found_by_det_afl > 0 {
                stats.bits_found_by_det_afl += state.fuzzer.bits_found_by_det_afl;
                state.fuzzer.bits_found_by_det_afl = 0;
            }
            if state.fuzzer.bits_found_by_splice > 0 {
                stats.bits_found_by_splice += state.fuzzer.bits_found_by_splice;
                state.fuzzer.bits_found_by_splice = 0;
            }
            if state.fuzzer.bits_found_by_havoc_rec > 0 {
                stats.bits_found_by_havoc_rec += state.fuzzer.bits_found_by_havoc_rec;
                state.fuzzer.bits_found_by_havoc_rec = 0;
            }
            if state.fuzzer.bits_found_by_min_rec > 0 {
                stats.bits_found_by_min_rec += state.fuzzer.bits_found_by_min_rec;
                state.fuzzer.bits_found_by_min_rec = 0;
            }
        }
    }
    //Else only use generation and no feedback
    else {
        loop {
            //If subprocess dies restart forkserver
            if state.generate_random("START").is_err() {
                let args = vec![];
                let fuzzer = Fuzzer::new(
                    path_to_bin_target.clone(),
                    args,
                    global_state.clone(),
                    config.dump_mode,
                    config.path_to_workdir.clone(),
                ).expect("RAND_574815774");
                state = FuzzingState::new(fuzzer, config.clone(), cks.clone());
                state.ctx = ctx.clone();
                old_execution_count = 0;
                old_executions_per_sec = 0;
            }
            let mut stats = global_state.lock().expect("RAND_1393236711");
            stats.execution_count += state.fuzzer.execution_count - old_execution_count;
            old_execution_count = state.fuzzer.execution_count;
            stats.average_executions_per_sec += state.fuzzer.average_executions_per_sec as u32;
            stats.average_executions_per_sec -= old_executions_per_sec;
            old_executions_per_sec = state.fuzzer.average_executions_per_sec as u32;
            if state.fuzzer.bits_found_by_gen > 0 {
                stats.bits_found_by_gen += state.fuzzer.bits_found_by_gen;
                state.fuzzer.bits_found_by_gen = 0;
            }
        }
    }
}

fn main() {
    //Parse parameters
    let matches = App::new("gramfuzz")
        .about("Grammar fuzzer")
        .arg(Arg::with_name("config")
             .short("g")
             .value_name("CONFIG")
             .takes_value(true)
             .help("Path to configuration file")
             .default_value("config.ron"))
        .arg(Arg::with_name("dumb")
             .short("d")
             .help("Don't use fancy calculations to generate trees (dumb mode)"))
        .arg(Arg::with_name("grammar")
             .help("Overwrite the grammar file specified in the CONFIG"))
        .get_matches();

    let dumb = matches.is_present("dumb");
    let config_file_path = matches.value_of("config")
        .expect("the path to the configuration file has a default value");

    println!(
        "{} Starting Fuzzing...",
        othertime::now()
            .strftime("[%Y-%m-%d] %H:%M:%S")
            .expect("RAND_1939191497")
    );

    //Set Config
    let mut config_file = File::open(&config_file_path).expect("cannot read config file");
    let mut config_file_contents = String::new();
    config_file
        .read_to_string(&mut config_file_contents)
        .expect("RAND_1413661228");
    let config: Config = ron::de::from_str(&config_file_contents).expect("Failed to deserialize");

    let shared = Arc::new(Mutex::new(GlobalSharedState::new(
        config.path_to_workdir.clone(),
    )));
    let shared_chunkstore = Arc::new(ChunkStoreWrapper::new());

    //Deserialize old State
    let queue_file_path = config.path_to_workdir.to_owned() + "saved_queue.ron";
    let bitmaps_file_path = config.path_to_workdir.to_owned() + "saved_bitmaps.ron";
    let chunkstore_file_path = config.path_to_workdir.to_owned() + "saved_chunkstore.ron";
    //if Path::new(&queue_file_path).is_file() {
    //    print!(
    //        "{} Found old Queue...",
    //        othertime::now()
    //            .strftime("[%Y-%m-%d] %H:%M:%S")
    //            .expect("RAND_386392372")
    //    );
    //    let mut sf = File::open(&queue_file_path).expect("cannot read saved queue file");
    //    let mut queue_as_string = String::new();
    //    sf.read_to_string(&mut queue_as_string)
    //        .expect("RAND_3228814687");
    //    shared.lock().expect("RAND_815997224").queue =
    //        ron::de::from_str(&queue_as_string).expect("Failed to deserialize queue");
    //    shared.lock().expect("RAND_1787917030").queue.work_dir = config.path_to_workdir.clone();
    //    println!("queue loaded");
    //}
    //if Path::new(&bitmaps_file_path).is_file() {
    //    print!(
    //        "{} Found old Bitmaps...",
    //        othertime::now()
    //            .strftime("[%Y-%m-%d] %H:%M:%S")
    //            .expect("RAND_3341541186")
    //    );
    //    let mut sf_bitmaps =
    //        File::open(&bitmaps_file_path).expect("cannot read saved bitmaps file");
    //    let mut bitmap_as_string = String::new();
    //    sf_bitmaps
    //        .read_to_string(&mut bitmap_as_string)
    //        .expect("RAND_1230001140");
    //    shared.lock().expect("RAND_4074891543").bitmaps =
    //        ron::de::from_str(&bitmap_as_string).expect("Failed to deserialize bitmap");
    //    println!("bitmaps loaded");
    //}
    //if Path::new(&chunkstore_file_path).is_file() {
    //    print!(
    //        "{} Found old Chunkstore...",
    //        othertime::now()
    //            .strftime("[%Y-%m-%d] %H:%M:%S")
    //            .expect("RAND_334262000")
    //    );
    //    let mut sf_chunkstore =
    //        File::open(&chunkstore_file_path).expect("cannot read saved chunkstore file");
    //    let mut chunkstore_as_string = String::new();
    //    sf_chunkstore
    //        .read_to_string(&mut chunkstore_as_string)
    //        .expect("RAND_3791229501");
    //    *shared_chunkstore
    //        .chunkstore
    //        .write()
    //        .expect("RAND_1421615953") =
    //        ron::de::from_str(&chunkstore_as_string).expect("Failed to deserialize chunkstore");
    //    println!("chunkstore loaded");
    //}

    //Generate rules using a grammar or deserialize saved context
    let mut my_context;
    let grammar_path = matches.value_of("grammar")
        .unwrap_or(&config.path_to_grammar)
        .to_owned();
    //let serialized_context_path = grammar_path.clone() + ".gfc";
    //let mut maybe_serialized_context = None;
    //Calculate string of grammar file
    let mut gf = File::open(grammar_path.clone()).expect("cannot open grammar file");
    let mut content = String::new();
    gf.read_to_string(&mut content)
        .expect("cannot read grammar file");
    let mut s = DefaultHasher::new();
    content.hash(&mut s);
    let hash = s.finish();
    //Deserialize saved context if the granmmar did not change (hash value still the same)
    //if Path::new(&serialized_context_path).is_file() {
    //    println!("Found saved context...");
    //    let mut cf = File::open(&serialized_context_path).expect("cannot read saved context file");
    //    let mut context_as_string = String::new();
    //    cf.read_to_string(&mut context_as_string)
    //        .expect("RAND_33259161");
    //    let serialized_context: SerializableContext =
    //        ron::de::from_str(&context_as_string).expect("Failed to deserialize context");
    //    //Check if file changed
    //    if hash != serialized_context.hash_of_original {
    //        println!("Grammar changed! Generating new context...");
    //    } else {
    //        maybe_serialized_context = Some(serialized_context);
    //    }
    //}
    //if let Some(serialized_context) = maybe_serialized_context {
    //    my_context = Context::from_serialized_context(serialized_context, true, dumb);
    //    println!("imported saved context!")
    //}
    //Create new Context and saved it
    //else {
        let mut my_parser = antlr_parser::AntlrParser::new();
        my_context = Context::with_dump(dumb);
        if grammar_path.ends_with(".json") {
            let gf = File::open(grammar_path).expect("cannot read grammar file");
            let rules: Vec<Vec<String>> =
                serde_json::from_reader(&gf).expect("cannot parse grammar file");
            let root = "{".to_string() + &rules[0][0] + "}";
            my_context.add_rule("START", &root);
            for rule in rules {
                my_context.add_rule(&rule[0], &rule[1]);
            }
        } else if grammar_path.ends_with(".g4") {
            my_parser.parse_antlr_grammar(&grammar_path);
            let root = "{".to_string() + &my_parser.rules[0].0 + "}";
            my_context.add_rule("START", &root);
            for rule in my_parser.rules {
                my_context.add_rule(&rule.0, &rule.1);
            }
        } else {
            panic!("Unknown grammar type");
        }
        my_context.initialize(config.max_tree_size, true);
        //Save context
        //let mut cf = File::create(&serialized_context_path).expect("cannot create context file");
        //let serializable_context: SerializableContext =
        //    my_context.create_serializable_context(hash);
        //cf.write_all(
        //    ron::ser::to_string(&serializable_context)
        //        .expect("Serialization of Context failed!")
        //        .as_bytes(),
        //).expect("Writing to context file failed");
    //}

    //Create output folder
    fs::create_dir_all(format!("{}/outputs", config.path_to_workdir)).expect("Could not create outputs folder");
    let signaled_dir = config.path_to_workdir.clone() + "outputs/signaled";
    let queue_dir = config.path_to_workdir.clone() + "outputs/queue";
    let timeout_dir = config.path_to_workdir.clone() + "outputs/timeout";
    let dump_dir = config.path_to_workdir.clone() + "outputs/dumped_inputs";
    fs::create_dir_all(signaled_dir).expect("Could not create singaled folder");
    fs::create_dir_all(timeout_dir).expect("Could not create timeout folder");
    fs::create_dir_all(queue_dir).expect("Could not create queue folder");
    if config.dump_mode {
        fs::create_dir_all(dump_dir).expect("Could not create queue folder");
    }

    let clone = shared.clone();
    let clone_of_chunkstore = shared_chunkstore.clone();
    let config_clone = config.clone();
    //Start fuzzing threads
    let mut thread_number = 0;
    let threads = (0..config.number_of_threads).map(|_| {
        let state = shared.clone();
        let config = config.clone();
        let ctx = my_context.clone();
        let cks = shared_chunkstore.clone();
        thread_number += 1;
        thread::Builder::new()
            .name(format!("fuzzer_{}", thread_number))
            .stack_size(config.thread_size)
            .spawn(move || fuzzing_thread(state, config, ctx, cks))
    });

    //Start status thread
    let status_thread = {
        let config = config.clone();
        let global_state = shared.clone();
        let shared_cks = shared_chunkstore.clone();
        thread::Builder::new()
            .name("status_thread".to_string())
            .spawn(move || {
                let start_time = Instant::now();
                thread::sleep(time::Duration::from_secs(1));
                print!("{}[2J", 27 as char);
                print!("{}[H", 27 as char);
                loop {
                    let execution_count;
                    let average_executions_per_sec;
                    let queue_len;
                    let bits_found_by_gen;
                    let bits_found_by_min;
                    let bits_found_by_min_rec;
                    let bits_found_by_det;
                    let bits_found_by_det_afl;
                    let bits_found_by_splice;
                    let bits_found_by_havoc;
                    let bits_found_by_havoc_rec;
                    let last_found_asan;
                    let last_found_sig;
                    let last_timeout;
                    let total_found_asan;
                    let total_found_sig;
                    let state_saved;
                    {
                        let shared_state = global_state.lock().expect("RAND_597319831");
                        execution_count = shared_state.execution_count;
                        average_executions_per_sec = shared_state.average_executions_per_sec;
                        queue_len = shared_state.queue.len();
                        bits_found_by_gen = shared_state.bits_found_by_gen;
                        bits_found_by_min = shared_state.bits_found_by_min;
                        bits_found_by_min_rec = shared_state.bits_found_by_min_rec;
                        bits_found_by_det = shared_state.bits_found_by_det;
                        bits_found_by_det_afl = shared_state.bits_found_by_det_afl;
                        bits_found_by_splice = shared_state.bits_found_by_splice;
                        bits_found_by_havoc = shared_state.bits_found_by_havoc;
                        bits_found_by_havoc_rec = shared_state.bits_found_by_havoc_rec;
                        last_found_asan = shared_state.last_found_asan.clone();
                        last_found_sig = shared_state.last_found_sig.clone();
                        last_timeout = shared_state.last_timeout.clone();
                        total_found_asan = shared_state.total_found_asan;
                        total_found_sig = shared_state.total_found_sig;
                        state_saved = shared_state.state_saved.clone();
                    }
                    let secs = start_time.elapsed().as_secs();
                    let minutes = secs / 60;
                    let hours = minutes / 60;
                    let days = hours / 24;
                    print!("{}[H", 27 as char);
                    if config.no_feedback_mode {
                        println!("-----------------------No-Feedback mode!---------------------");
                    }
                    println!(
                        "Run Time: {} days, {} hours, {} minutes, {} seconds       ",
                        days,
                        hours % 24,
                        minutes % 60,
                        secs % 60
                    );
                    println!(
                        "Execution Count:          {}                              ",
                        execution_count
                    );
                    println!(
                        "Executions per Sec:       {}                              ",
                        average_executions_per_sec
                    );
                    if config.no_feedback_mode == false {
                        println!(
                            "Left in queue:            {}                              ",
                            queue_len
                        );
                        let now = Instant::now();
                        while shared_cks.is_locked.load(Ordering::SeqCst) {
                            if now.elapsed().as_secs() > 30 {
                                panic!("Printing thread starved!");
                            }
                        }
                        println!(
                            "Trees in Chunkstore:      {}                              ",
                            shared_cks
                                .chunkstore
                                .read()
                                .expect("RAND_351823021")
                                .trees()
                        );
                    }
                    println!("------------------------------------------------------    ");
                    println!(
                        "Last ASAN crash:          {}                              ",
                        last_found_asan
                    );
                    println!(
                        "Last SIG crash:           {}                              ",
                        last_found_sig
                    );
                    println!(
                        "Last Timeout:             {}                              ",
                        last_timeout
                    );
                    println!(
                        "Total ASAN crashes:       {}                              ",
                        total_found_asan
                    );
                    println!(
                        "Total SIG crashes:        {}                              ",
                        total_found_sig
                    );
                    println!("------------------------------------------------------    ");
                    println!(
                        "New paths found by Gen:          {}                       ",
                        bits_found_by_gen
                    );
                    if config.no_feedback_mode == false {
                        println!(
                            "New paths found by Min:          {}                       ",
                            bits_found_by_min
                        );
                        println!(
                            "New paths found by Min Rec:      {}                       ",
                            bits_found_by_min_rec
                        );
                        println!(
                            "New paths found by Det:          {}                       ",
                            bits_found_by_det
                        );
                        println!(
                            "New paths found by Det Afl:      {}                       ",
                            bits_found_by_det_afl
                        );
                        println!(
                            "New paths found by Splice:       {}                       ",
                            bits_found_by_splice
                        );
                        println!(
                            "New paths found by Havoc:        {}                       ",
                            bits_found_by_havoc
                        );
                        println!(
                            "New paths found by Havoc Rec:    {}                       ",
                            bits_found_by_havoc_rec
                        );
                    }
                    println!("------------------------------------------------------    ");
                    println!(
                        "Last time state saved: {}                                 ",
                        state_saved
                    );
                    println!("------------------------------------------------------    ");
                    //println!("Global bitmap: {:?}", global_state.lock().expect("RAND_1887203473").bitmaps.get(&false).expect("RAND_1887203473"));
                    thread::sleep(time::Duration::from_secs(1));
                }
            })
            .expect("RAND_3541874337")
    };

    //Start saving thread
    if config_clone.save_state {
        let save_thread = {
            let global_state = shared.clone();
            thread::Builder::new()
                .name("state_saver".to_string())
                .stack_size(config_clone.save_thread_size)
                .spawn(move || {
                    //let mut id = "1";
                    loop {
                        thread::sleep(time::Duration::from_secs(config_clone.save_intervall));

                        let mut of =
                            File::create(&queue_file_path).expect("cannot create output file");
                        of.write_all(
                            ron::ser::to_string(&clone.lock().expect("RAND_372393424").queue)
                                .expect("Serialization of Queue failed!")
                                .as_bytes(),
                        ).expect("Writing to queue file failed");

                        let mut of_bitmap =
                            File::create(&bitmaps_file_path).expect("cannot create output file");
                        of_bitmap
                            .write_all(
                                ron::ser::to_string(
                                    &clone.lock().expect("RAND_1525717184").bitmaps,
                                ).expect("Serialization of Bitmaps failed!")
                                    .as_bytes(),
                            )
                            .expect("Writing to bitmap file failed");

                        let mut of_chunkstore = File::create(
                            &/*(*/chunkstore_file_path, /*.to_owned()+id)*/
                        ).expect("cannot create output file");
                        of_chunkstore
                            .write_all(
                                ron::ser::to_string(
                                    &(*clone_of_chunkstore
                                        .chunkstore
                                        .read()
                                        .expect("RAND_4283477146")),
                                ).expect("Serialization of Chunkstore failed!")
                                    .as_bytes(),
                            )
                            .expect("Writing to Chunkstore file failed");
                        //id = if id == "1" { "0" } else { "1" };
                        {
                            global_state.lock().expect("RAND_3289262969").state_saved =
                                strftime("[%Y-%m-%d] %H:%M:%S", &othertime::now())
                                    .expect("RAND_3227256997");
                        }
                    }
                })
                .expect("RAND_2513095620")
        };

        for t in threads.collect::<Vec<_>>().into_iter() {
            t.expect("RAND_1599964266").join().expect("RAND_1599964266");
        }
        save_thread.join().expect("RAND_2798744238");
    } else {
        for t in threads.collect::<Vec<_>>().into_iter() {
            t.expect("RAND_2698731594").join().expect("RAND_2698731594");
        }
    }
    status_thread.join().expect("RAND_399292929");
}
