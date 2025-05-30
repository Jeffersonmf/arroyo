import { Dispatch, useEffect, useRef, useState } from 'react';
import { CreateConnectionState } from './CreateConnection';
import { Alert, AlertIcon, Box, Button, List, ListItem, Stack } from '@chakra-ui/react';
import * as monaco from 'monaco-editor/esm/vs/editor/editor.api';
import { post } from '../../lib/data_fetching';
import { formatError } from '../../lib/util';

export function JsonSchemaEditor({
  state,
  setState,
  next,
}: {
  state: CreateConnectionState;
  setState: Dispatch<CreateConnectionState>;
  next: () => void;
}) {
  const [editor, setEditor] = useState<monaco.editor.IStandaloneCodeEditor | null>(null);
  const monacoEl = useRef(null);
  const created = useRef(false);
  const [errors, setErrors] = useState<Array<string> | null>(null);
  const [testing, setTesting] = useState<boolean>(false);
  const [tested, setTested] = useState<string | undefined>();

  const valid = tested == editor?.getValue() && errors?.length == 0;

  const testSchema = async () => {
    console.log('testing schema');
    setTesting(true);
    setErrors(null);
    const { error } = await post('/v1/connection_tables/schemas/test', {
      body: state.schema!,
    });
    if (error) {
      setErrors([formatError(error)]);
    } else {
      setErrors([]);
    }

    setTested(editor?.getValue());
    setTesting(false);
  };

  let errorBox = null;
  if (errors != null) {
    if (errors.length == 0) {
      errorBox = (
        <Box>
          <Alert status="success">
            <AlertIcon />
            The schema is valid
          </Alert>
        </Box>
      );
    } else {
      errorBox = (
        <Box>
          <Alert status="error">
            <AlertIcon />
            <List>
              {errors.map(e => (
                <ListItem key={e}>{e}</ListItem>
              ))}
            </List>
          </Alert>
        </Box>
      );
    }
  }

  useEffect(() => {
    if (
      monacoEl &&
      !editor &&
      !created.current &&
      state.schema?.format?.json?.unstructured === false
    ) {
      let e = monaco.editor.create(monacoEl.current!, {
        // value: state.schema?.definition,
        language: 'json',
        theme: 'vs-dark',
        minimap: {
          enabled: false,
        },
      });

      e?.getModel()?.onDidChangeContent(_ => {
        setState({
          ...state,
          schema: {
            ...state.schema,
            definition: { json_schema: e.getValue() },
            fields: [],
            format: { json: { confluentSchemaRegistry: false } },
          },
        });
      });

      created.current = true;
      setEditor(e);
    }

    return () => editor?.dispose();
  }, []);

  return (
    <Stack spacing={4}>
      <Box marginTop={5} width="100%">
        <div className="editor" ref={monacoEl}></div>
      </Box>

      {errorBox}

      {valid ? (
        <Button width={150} colorScheme="green" onClick={next}>
          Next
        </Button>
      ) : (
        <Button width={150} variant="primary" isLoading={testing} onClick={testSchema}>
          Validate
        </Button>
      )}
    </Stack>
  );
}
