

!macro NSIS_HOOK_POSTUNINSTALL
  MessageBox MB_YESNO|MB_ICONQUESTION "vidcull의 설정·인덱스·캐시 데이터도 삭제할까요?$\n$\n영상 원본 파일은 영향받지 않습니다.$\n($LOCALAPPDATA\vidcull)" /SD IDNO IDNO vidcull_keep_data
  RMDir /r "$LOCALAPPDATA\vidcull"
  vidcull_keep_data:
!macroend
