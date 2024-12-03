import { render, createRef, Component } from 'preact';
import { signal, Signal } from '@preact/signals';
import { EditorState } from '@codemirror/state';
import { EditorView, basicSetup } from 'codemirror';

import './style.css';

enum FileKind {
    Editor,
}

type FileId = string;
type ImageId = string;

class File {
    static _id = 0;

    id: string;
    path: string;
    kind: FileKind;
    editorState?: EditorState;
    data: string; // TODO or arraybuffer or whatever

    static _next_id(): number {
        File._id += 1;
        return File._id;
    }

    constructor(path, kind, data) {
        this.id = File._next_id().toString();
        this.path = path;
        this.kind = kind;
        this.data = data;
        if (this.kind == FileKind.Editor) {
            this.editorState = EditorState.create({
                doc: data,
            });
        }
    }

    static makeTextFile(path, data): File {
        return new File(path, FileKind.Editor, data);
    }

    displayName() {
        return this.path;
    }
};

namespace Api {
    export type Image = {
        links: {
            runi: string,
            upstream: string,
        },
        info: {
            digest: string,
            repository: string,
            registry: string,
            tag: string,
        },
        config: {
            created: string,
            architecture: string,
            os: string,
            config: {
                Cmd?: string[],
                Entrypoint?: string[],
                Env?: string[],
            },
            rootfs: {type: string, diff_ids: string[]}[],
            history: any, // todo
        },
    };
}

class FileStore {
    files: Map<FileId, File> = new Map();
    active?: string = null;

    constructor(files?: Map<FileId, File>, active?: FileId) {
        this.files = files ?? new Map();
        this.active = active ?? null;
    }

    addTextFile(path, data): FileStore {
        let f = File.makeTextFile(path, data);
        console.log('before', this.files);
        let files = new Map(this.files);
        files.set(f.id, f);
        console.log('after', this.files);
        return new FileStore(files, this.active);
    }

    setActive(file: File): FileStore {
        // do we have to copy this?
        this.active = file.id;
        return this;
    }
}

type AppState = {
    images: Api.Image[],
    imagesMap: Map<ImageId, Api.Image>,
    stdin?: FileId,
    selectedImage?: ImageId,
}

//const initialFiles: File[] = [
//  {
//    name: 'main.sh',
//    data: 'echo "hello world"',
//  },
//];

// signals.
// is this good or terrible? I don't know
class Editor extends Component {
    ref = createRef();
    editor?: EditorView;
    props: {
        readOnly: boolean,
        placeholder?: string,
    };
    state: {
        store: FileStore,
    };

    constructor({readOnly, placeholder}: {readOnly?: boolean, placeholder?: string}) {
        super({placeholder,readOnly});
        this.state = {
            store: new FileStore(),
        };
    }

    componentDidMount() {
        this.editor = new EditorView({
          extensions: [
              basicSetup,
              ...(this.props.readOnly ? [EditorState.readOnly.of(true)] : []),
          ],
          parent: this.ref.current,
        });
    }

    addTextFileAndActivate(path, data) {
        let store = this.state.store.addTextFile(path, data);
        store.active = store.files.keys().next().value;
        console.log(store);
        this.setState({store: store});
    }

    editFile(file: File) {
        this.setState({store: this.state.store.setActive(file)});
    }

    // this.props, this.state
    render({placeholder,readOnly}, {store}) {
        let tabs = Array.from(store.files.values().map(file => {
            let className = 'tab';
            if (store.active == file.id) {
                className += ' selected';
            }
            return (
                <button
                    className={className}
                    key={file.id} onClick={() => this.editFile(file)}>
                    {file.displayName()}
                </button>
            );
        }));
        if (this.editor && store.active) {
            let f = store.files.get(store.active);
            if (f.kind == FileKind.Editor) {
                this.editor.setState(f.editorState);
            }
        }
        return (
            <div class="editorContainer">
                {tabs}
                <div class="cmContainer" ref={this.ref}></div>
            </div>
        );
    }
}

//class ImageForm extends Component {
//    render() {
//        //let imageOptions = s_images.value.images?.map(({info,links}) => {
//        //    let name = `${info.registry}/${info.repository}/${info.tag}`;
//        //    return <option key={links.runi} value={links.runi}>{name}</option>;
//        //});
//        //let stdinOptions = inputModelStore.models.value.map(m => {
//        //    let name = inputModelStore.modelName(m);
//        //    return <option key={m.uri.path} value={name}>{name}</option>;
//        //});
//        //let entrypoint = s_chosenImage.value?.config.config.Entrypoint || 'Entrypoint';
//        //let cmd = s_chosenImage.value?.config.config.cmd || 'Cmd';
//
//        let entrypoint = 'Entrypoint';
//        let cmd = 'Cmd';
//        return (
//            <>
//            <select name="image" onChange={(event) => { s_chosenImage.value = event.currentTarget.value; }}>
//                {imageOptions}
//            </select>
//            <input type="text" name="entrypoint" placeholder={entrypoint} />
//            <input type="text" name="cmd" placeholder={cmd} />
//            <select name="stdin" onChange={(event) => { s_chosenImage.value = event.currentTarget.value; }}>
//                <option value="">/dev/null</option>
//                {stdinOptions}
//            </select>
//            </>
//        );
//    }
//}


class App extends Component {
    state: AppState;
    inputEditor = createRef();
    outputEditor = createRef();

    constructor() {
        super();
        this.state = {
            images: [],
            imagesMap: new Map(),
            selectedImage: null,
            stdin: null,
        };

        //this.inputEditor.addTextFileAndActivate('test.sh', 'echo "hello world"');
        //let inputs = this.state.inputs.addTextFile('test.sh', 'echo "hello world"');
        //inputs.active = inputs.files.keys().next().value;
        //this.setState({inputs: inputs});
    }

    componentDidMount() {
        // if you execute these back to back they don't both get applied...
        this.inputEditor.current.addTextFileAndActivate('test.sh', 'echo "hello world"');
        setTimeout(
            () => {
                this.inputEditor.current.addTextFileAndActivate('test2.sh', 'echo "boooo"');
            }, 1000);
    }

    async run(event) {
        event.preventDefault();
        //console.log('hi this should run');
        //// okay I need to get the current image, entrypoint, cmd, stdin
        //// the image should already be the uri
        //
        //if (s_chosenImage.value !== null) {
        //    let data = {};
        //    // TODO get entrypoint and cmd from child
        //    data.cmd = ["sh", "-c", "echo hi"];
        //    let req = new Request(window.location.origin + s_chosenImage.value, {
        //        method: 'POST',
        //        body: JSON.stringify(data),
        //        headers: {
        //            'Content-type': 'application/json',
        //        }
        //    });
        //    const response = await fetch(req);
        //    if (response.ok) {
        //        const json = await response.json();
        //        let m = outputModelStore.getOrCreateModel('output.json', JSON.stringify(json, null, '  '));
        //        outputEditor.editFile(m.uri);
        //    } else {
        //        console.error(response);
        //    }
        //} else {
        //    console.warn('cannot execute without a chosen image');
        //}
    }

    // this.props, this.state
    render({}, {}) {
        return <div>
            <form>
                <input className="mono" type="text" placeholder="env $entrypoint $cmd < /dev/null" />
                <button onClick={this.run}>Run</button>
            </form>

            <div id="editorSideBySide">
                <Editor ref={this.inputEditor} />
                <Editor
                    ref={this.outputEditor}
                    readOnly={true}
                    placeholder={PLACEHOLDER_DIRECTIONS} />
            </div>
        </div>
    }
}

const PLACEHOLDER_DIRECTIONS = `Run something to see the output...`;
// TODO something about the args going in not being compat with the Component type signature
render(<App/>, document.getElementById('app'));

//const response = await fetch(window.location.origin + '/api/v1/images');
//console.log(response);
//if (response.ok) {
//    const json = await response.json();
//    s_images.value = json;
//    if (json.images?.length > 0) {
//        s_chosenImage.value = json.images[0].links.runi;
//    }
//} else {
//    console.error(response);
//}
//
//<Editor
//  height="80vh"
//  theme="vs-dark"
//  path={file.name}
//  defaultLanguage={file.language}
//  defaultValue={file.value}
///>
//function getOrCeateFile(path, text) {
//    let uri = monaco.Uri.file(path);
//    return monaco.editor.getModel(uri) || monaco.editor.createModel(text, undefined, uri);
//}

